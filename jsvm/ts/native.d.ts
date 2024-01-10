interface String {
  /**
   * UTF-8 encode the current string.
   */
  toBuffer(): Uint8Array;
}

interface StringConstructor {
  /**
   * Create a string from UTF-8 buffer (with replacement cheracter for invalid sequences)
   */
  fromBuffer(buffer: Uint8Array): string;
}

interface Uint8Array {
  /**
   * UTF-8 decode the current buffer.
   */
  decode(): string;
}

declare module "_aici" {
  type Buffer = Uint8Array;

  /**
   * Return token indices for a given string (or byte sequence).
   */
  function tokenize(text: string | Buffer): number[];

  /**
   * Return byte (~string) representation of a given list of token indices.
   */
  function detokenize(tokens: number[]): Buffer;

  /**
   * Return identifier of the current sequence.
   * Most useful with fork_group parameter in mid_process() callback.
   * Best use aici.fork() instead.
   */
  function selfSeqId(): number;

  /**
   * Print out a message of the error and stop the program.
   */
  function panic(error: any): never;

  /**
   * Get the value of a shared variable.
   */
  function getVar(name: string): Buffer | null;

  /**
   * Set the value of a shared variable.
   */
  function setVar(name: string, value: string | Buffer): void;

  /**
   * Append to the value of a shared variable.
   */
  function appendVar(name: string, value: string | Buffer): void;

  /**
   * Index of the end of sequence token.
   */
  function eosToken(): number;

  /**
   * UTF-8 encode
   */
  function stringToBuffer(s: string): Buffer;

  /**
   * UTF-8 decode (with replacement cheracter for invalid sequences)
   */
  function bufferToString(b: Buffer): string;

  /**
   * Return a string like `b"..."` that represents the given buffer.
   */
  function bufferRepr(b: Buffer): string;

  /**
   * Represents a set of tokens.
   * The value is true at indices corresponding to tokens in the set.
   */
  class TokenSet {
    /**
     * Create an empty set (with .length set to the total number of tokens).
     */
    constructor();

    add(t: number): void;
    delete(t: number): void;
    has(t: number): boolean;
    clear(): void;

    /**
     * Number of all tokens (not only in the set).
     */
    length: number;

    /**
     * Include or exclude all tokens from the set.
     */
    setAll(value: boolean): void;
  }

  /**
   * Initialize a constraint that allows any token.
   */
  class Constraint {
    constructor();

    /**
     * Check if the constraint allows the generation to end at the current point.
     */
    eosAllowed(): boolean;

    /**
     * Check if the constraint forces the generation to end at the current point.
     */
    eosForced(): boolean;

    /**
     * Check if token `t` is allowed by the constraint.
     */
    tokenAllowed(t: number): boolean;

    /**
     * Update the internal state of the constraint to reflect that token `t` was appended.
     */
    appendToken(t: number): void;

    /**
     * Set ts[] to True at all tokens that are allowed by the constraint.
     */
    allowTokens(ts: TokenSet): void;
  }

  /**
   * A constraint that allows only tokens that match the regex.
   * The regex is implicitly anchored at the start and end of the generation.
   */
  function regexConstraint(pattern: string): Constraint;

  /**
   * A constraint that allows only tokens that match the specified yacc-like grammar.
   */
  function cfgConstraint(yacc_grammar: string): Constraint;

  /**
   * A constraint that allows only word-substrings of given string.
   */
  function substrConstraint(template: string, stop_at: string): Constraint;
}