use anyhow::{anyhow, Error as E, Result};
use candle::{DType, Device, IndexOp, Tensor};
use candle_nn::VarBuilder;
use hf_hub::{
    api::sync::{Api, ApiRepo},
    RepoType,
};
use std::{collections::HashSet, fmt::Display, path::PathBuf, sync::Arc, time::Instant};
use tokenizers::Tokenizer;

use candle_transformers::models::llama as llama_ref;

use crate::LogitsProcessor;
use crate::{
    cache_engine::CacheEngine,
    config::{
        CacheConfig, ModelConfig, ParallelConfig, RllmConfig, SamplingParams, SchedulerConfig,
    },
    scheduler::SchedulerOutputs,
    seq::{FinishReason, RequestOutput, SchedulingPhase, SequenceGroup, Token},
    to_offsets,
};
use crate::{
    llama::{Llama, LlamaConfig},
    LoaderArgs,
};
use crate::{
    scheduler::Scheduler,
    seq::{BatchInfo, SeqId, Sequence, StepType},
};

enum Repo {
    Api(ApiRepo),
    Local(String),
}

impl Repo {
    fn from(args: &LoaderArgs) -> Result<Repo> {
        match &args.local_weights {
            Some(path) => Ok(Repo::Local(path.to_owned())),
            None => {
                let api = Api::new()?;
                let model_id = args
                    .model_id
                    .clone()
                    .unwrap_or_else(|| "NousResearch/Llama-2-7b-hf".to_string());
                let revision = args.revision.clone().unwrap_or("main".to_string());
                let api = api.repo(hf_hub::Repo::with_revision(
                    model_id,
                    RepoType::Model,
                    revision,
                ));
                Ok(Repo::Api(api))
            }
        }
    }

    fn get(&self, filename: &str) -> Result<PathBuf> {
        match self {
            Repo::Api(api) => api.get(filename).map_err(E::msg),
            Repo::Local(path) => Ok((path.to_owned() + filename).into()),
        }
    }

    fn read(&self, filename: &str) -> Result<Vec<u8>> {
        std::fs::read(self.get(filename)?).map_err(E::msg)
    }
}

impl Display for Repo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Repo::Api(api) => write!(f, "{}", api.url("")),
            Repo::Local(path) => write!(f, "{}", path),
        }
    }
}

pub enum Model {
    Llama(Llama),
    Reference(llama_ref::Llama),
}

impl Model {
    pub fn forward(&self, info: &BatchInfo) -> Result<Tensor> {
        match self {
            Model::Llama(llama) => Ok(llama.forward(info)?),
            Model::Reference(llama) => {
                let index_pos = info.positions.i(0..1)?.to_vec1::<i64>()?[0];
                let input = info.tokens.unsqueeze(0)?;
                Ok(llama.forward(&input, index_pos as usize)?)
            }
        }
    }
}

pub struct RllmEngine {
    pub tokenizer: Tokenizer,
    pub model: Model,
    seq_id: SeqId,
    step_no: usize,
    cache_engine: CacheEngine,
    #[allow(dead_code)]
    pub alt: usize,
    pub device: Device,
    pub eos_token_id: u32,

    scheduler: Scheduler,
}

impl RllmEngine {
    pub fn load(args: LoaderArgs) -> Result<RllmEngine> {
        let device = Device::new_cuda(0)?;
        let dtype = DType::BF16;

        let repo = Repo::from(&args)?;
        log::info!("loading the model weights from {}", repo);

        let tokenizer_filename = repo.get("tokenizer.json")?;

        let json_config: LlamaConfig = serde_json::from_slice(&repo.read("config.json")?)?;
        let model_config: ModelConfig = json_config.into_config();

        let mut rllm_config = RllmConfig {
            model: model_config.clone(),
            parallel: ParallelConfig::single(),
            cache: CacheConfig::default(),
            scheduler: SchedulerConfig::new(2560, 256, model_config.max_sequence_length),
            dtype,
            device: device.clone(),
        };

        // TODO infer these
        let elt_size = CacheEngine::get_cache_block_size(&rllm_config);
        let cache_mem = 4 << 30; // 4GiB
        rllm_config.cache.num_cpu_blocks = Some(cache_mem / elt_size);
        rllm_config.cache.num_gpu_blocks = Some(cache_mem / elt_size);

        let st_index: serde_json::Value =
            serde_json::from_slice(&repo.read("model.safetensors.index.json")?)?;

        let entries = st_index["weight_map"]
            .as_object()
            .unwrap()
            .values()
            .map(|v| v.as_str().unwrap().to_owned());

        let h = HashSet::<String>::from_iter(entries);
        let mut filenames = h.iter().collect::<Vec<_>>();
        filenames.sort();
        let filenames = filenames
            .iter()
            .map(|f| repo.get(f))
            .collect::<Result<Vec<_>>>()?;

        log::info!("building the model");

        let vb = unsafe { VarBuilder::from_mmaped_safetensors(&filenames, dtype, &device)? };
        let tokenizer = Tokenizer::from_file(tokenizer_filename).map_err(anyhow::Error::msg)?;

        let eos_token_id = tokenizer
            .token_to_id("</s>")
            .ok_or(anyhow!("</s> not found"))?;

        let model = if args.use_reference {
            let config: llama_ref::LlamaConfig =
                serde_json::from_slice(&repo.read("config.json")?)?;
            let use_flash_attn = true;
            let config = config.into_config(use_flash_attn);
            let use_kv_cache = true;
            let cache = llama_ref::Cache::new(use_kv_cache, dtype, &config, &device)?;
            let llama = llama_ref::Llama::load(vb, &cache, &config)?;
            Model::Reference(llama)
        } else {
            let llama = Llama::load(vb, &model_config)?;
            Model::Llama(llama)
        };

        log::info!("model loaded");

        let rllm_config = Arc::new(rllm_config);
        let scheduler = Scheduler::new(rllm_config.clone());
        let cache_engine = CacheEngine::new(rllm_config.clone());

        Ok(RllmEngine {
            tokenizer,
            model,
            seq_id: 1,
            step_no: 0,
            device,
            eos_token_id,
            alt: args.alt,
            scheduler,
            cache_engine,
        })
    }

    pub fn add_request(
        &mut self,
        request_id: String,
        prompt: &str,
        sampling_params: SamplingParams,
    ) -> Result<()> {
        let tokens = self
            .tokenizer
            .encode(prompt, true)
            .map_err(anyhow::Error::msg)?
            .get_ids()
            .to_vec();
        let seq = Sequence::new(self.seq_id, &tokens, self.scheduler.config.cache.block_size);
        self.seq_id += 1;

        let logits_processor = LogitsProcessor::new(&sampling_params);
        let sg = SequenceGroup {
            request_id,
            seqs: vec![seq],
            sampling_params,
            arrival_time: Instant::now(),
            logits_processor,
        };

        self.scheduler.add_seq_group(sg);

        Ok(())
    }

    fn generate_outputs(
        &self,
        logits: &Tensor,
        sched_out: &mut SchedulerOutputs,
    ) -> Result<Vec<RequestOutput>> {
        let mut outputs = Vec::new();
        let mut idx = 0;

        for sg in sched_out.next_seq_groups.iter_mut() {
            let mut outp = RequestOutput {
                request_id: sg.request_id.clone(),
                seq_outputs: Vec::new(),
            };
            for seq in sg.seqs.iter_mut() {
                if seq.sched_phase == SchedulingPhase::Running {
                    let logits = logits.i((idx, ..))?;
                    let next_token = sg.logits_processor.sample(&logits)?;
                    seq.tokens.push(next_token);
                    seq.step_type = StepType::Gen;
                    idx += 1;

                    if next_token == self.eos_token_id {
                        self.scheduler.finish_seq(seq, FinishReason::FoundEos);
                    } else if seq.get_gen_len() >= sg.sampling_params.max_tokens {
                        self.scheduler
                            .finish_seq(seq, FinishReason::MaxTokensReached);
                    }
                }
                outp.seq_outputs.push(seq.get_output());
            }
            outputs.push(outp);
        }

        Ok(outputs)
    }

    fn run_model(&mut self, sched_out: &mut SchedulerOutputs) -> Result<Vec<RequestOutput>> {
        if sched_out.is_empty() {
            log::debug!("no seqs to run");
            return Ok(Vec::new());
        }

        let mut issued_cache_op = false;
        if sched_out.blocks_to_swap_in.len() > 0 {
            self.cache_engine.swap_in(&sched_out.blocks_to_swap_in);
            issued_cache_op = true;
        }
        if sched_out.blocks_to_swap_out.len() > 0 {
            self.cache_engine.swap_out(&sched_out.blocks_to_swap_out);
            issued_cache_op = true;
        }
        if sched_out.blocks_to_copy.len() > 0 {
            self.cache_engine.copy(&sched_out.blocks_to_copy);
            issued_cache_op = true;
        }

        if issued_cache_op {
            self.cache_engine.wait_for_copy();
        }

        let info = self.build_batch_info(sched_out)?;

        log::trace!("batch_info #{}: {:?}", self.step_no, info);
        let logits = self.model.forward(&info)?;
        log::trace!("logits: {:?}", logits);

        self.generate_outputs(&logits, sched_out)
    }

    fn build_batch_info(&self, sched_out: &mut SchedulerOutputs) -> Result<BatchInfo> {
        let mut positions: Vec<i64> = Vec::new();
        let mut tokens: Vec<Token> = Vec::new();
        let mut seqlens_q = Vec::new();
        let mut seqlens_k = Vec::new();
        let mut gather_mapping: Vec<u32> = Vec::new();
        let mut slot_mapping: Vec<u32> = Vec::new();

        let max_seq = self.scheduler.config.model.max_sequence_length;

        for sg in sched_out.next_seq_groups.iter_mut() {
            for seq in sg.seqs.iter_mut() {
                if seq.sched_phase != SchedulingPhase::Running {
                    continue;
                }

                let seq_len = seq.tokens.len();
                let k_len = seq_len;
                let q_len = match seq.step_type {
                    StepType::Prompt => seq_len,
                    StepType::Fixed(len) => len,
                    StepType::Gen => 1,
                };
                let off = k_len - q_len;
                for idx in off..off + q_len {
                    assert!(idx < max_seq);
                    positions.push(idx as i64);
                    tokens.push(seq.tokens[idx]);
                    slot_mapping.push(seq.get_gpu_slot(idx) as u32);
                }
                for idx in 0..k_len {
                    gather_mapping.push(seq.get_gpu_slot(idx) as u32);
                }
                seqlens_q.push(q_len);
                seqlens_k.push(k_len);
            }
        }

        let device = &self.device;
        let (max_seqlen_q, seqlens_q) = to_offsets(&seqlens_q, device);
        let (max_seqlen_k, seqlens_k) = to_offsets(&seqlens_k, device);

        let positions = Tensor::new(positions.as_slice(), device)?;
        let tokens = Tensor::new(tokens.as_slice(), device)?;
        let slot_mapping = Tensor::new(slot_mapping.as_slice(), device)?;
        let gather_mapping = Tensor::new(gather_mapping.as_slice(), device)?;

        let kv_cache = self.cache_engine.get_gpu_cache();

        Ok(BatchInfo {
            tokens,
            positions,
            seqlens_q,
            seqlens_k,
            slot_mapping,
            gather_mapping,
            max_seqlen_q,
            max_seqlen_k,
            kv_cache,
        })
    }

    pub fn step(&mut self) -> Result<Vec<RequestOutput>> {
        self.step_no += 1;
        let mut sched_out = self.scheduler.schedule();
        log::trace!(
            "scheduled: {} groups, dropped: {}",
            sched_out.next_seq_groups.len(),
            sched_out.dropped_seq_groups.len()
        );
        let outputs = self.run_model(&mut sched_out);
        // we run step_finished() regardless if model failed
        self.scheduler.step_finished(sched_out);
        Ok(outputs?)
    }

    pub fn decode_seq(&self, tokens: &Vec<Token>) -> Result<String> {
        let generated = self
            .tokenizer
            .decode(tokens, true)
            .map_err(anyhow::Error::msg)?;
        Ok(generated)
    }

    pub fn generate(&mut self, prompt: &str, sampling_params: SamplingParams) -> Result<String> {
        let req_id = format!("R{}", self.step_no);
        self.add_request(req_id, prompt, sampling_params)?;

        let mut outputs = Vec::new();

        while self.scheduler.has_unfinished_seqs() {
            let outp = self.step()?;
            if !outp.is_empty() {
                assert!(outp.len() == 1);
                assert!(outp[0].seq_outputs.len() == 1);
                outputs = outp[0].seq_outputs[0].output_tokens.clone();
            }
        }

        Ok(self.decode_seq(&outputs)?)
    }
}