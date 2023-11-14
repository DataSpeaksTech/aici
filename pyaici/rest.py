import requests
import ujson
from typing import Optional

base_url = "http://127.0.0.1:8080/v1/"
log_level = 1
ast_module = ""

def upload_module(file_path: str) -> str:
    """
    Upload a WASM module to the server.
    Returns the module ID.
    """
    if log_level > 0:
        print("upload module... ", end="")
    with open(file_path, "rb") as f:
        resp = requests.post(base_url + "aici_modules", data=f)
        if resp.status_code == 200:
            d = resp.json()
            dd = d["data"]
            mod_id = dd["module_id"]
            if log_level > 0:
                print(
                    f"{dd['wasm_size']//1024}kB -> {dd['compiled_size']//1024}kB id:{mod_id[0:8]}"
                )
            return mod_id
        else:
            raise RuntimeError(
                f"bad response to model upload: {resp.status_code} {resp.reason}: {resp.text}"
            )


def completion(
    prompt,
    aici_module,
    aici_arg,
    temperature=0,
    max_tokens=200,
    n=1,
):
    json = {
        "model": "",
        "prompt": prompt,
        "max_tokens": max_tokens,
        "n": n,
        "temperature": temperature,
        "stream": True,
        "aici_module": aici_module,
        "aici_arg": aici_arg,
    }
    resp = requests.post(base_url + "completions", json=json, stream=True)
    if resp.status_code != 200:
        raise RuntimeError(
            f"bad response to completions: {resp.status_code} {resp.reason}: {resp.text}"
        )
    texts = [""] * n
    full_resp = []
    res = {
        "request": json,
        "response": full_resp,
        "text": texts,
        "error": None,
    }

    for line in resp.iter_lines():
        if res["error"]:
            break
        if not line:
            continue
        decoded_line: str = line.decode("utf-8")
        if decoded_line.startswith("data: {"):
            d = ujson.decode(decoded_line[6:])
            full_resp.append(d)
            for ch in d["choices"]:
                if "Previous WASM Error" in ch["logs"]:
                    res["error"] = "WASM error"
                idx = ch["index"]
                while len(texts) <= idx:
                    texts.append("")
                if idx == 0:
                    if log_level > 1:
                        l = ch["logs"].rstrip("\n")
                        if l:
                            print(l)
                        # print(f"*** TOK: '{ch['text']}'")
                    elif log_level > 0:
                        print(ch["text"], end="")
                texts[idx] += ch["text"]
        elif decoded_line == "data: [DONE]":
            if log_level > 0:
                print("[DONE]")
        else:
            raise RuntimeError(f"bad response line: {decoded_line}")

    return res
