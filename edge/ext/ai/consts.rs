/// Default ONNX weights for the `gte-small` embedding model. Overridable at
/// runtime via the `AI_GTE_SMALL_MODEL_URL` env var so flow is not pinned to
/// any particular host.
pub const GTE_SMALL_MODEL_URL_DEFAULT: &str =
  "https://huggingface.co/Supabase/gte-small/resolve/main/onnx/model.onnx";

/// Default tokenizer for the `gte-small` embedding model. Overridable via the
/// `AI_GTE_SMALL_TOKENIZER_URL` env var.
pub const GTE_SMALL_TOKENIZER_URL_DEFAULT: &str = "https://huggingface.co/Supabase/gte-small/resolve/main/tokenizer.json?download=true";

/// Resolve the `gte-small` model URL, honoring the `AI_GTE_SMALL_MODEL_URL`
/// override.
pub fn gte_small_model_url() -> String {
  std::env::var("AI_GTE_SMALL_MODEL_URL")
    .unwrap_or_else(|_| GTE_SMALL_MODEL_URL_DEFAULT.to_string())
}

/// Resolve the `gte-small` tokenizer URL, honoring the
/// `AI_GTE_SMALL_TOKENIZER_URL` override.
pub fn gte_small_tokenizer_url() -> String {
  std::env::var("AI_GTE_SMALL_TOKENIZER_URL")
    .unwrap_or_else(|_| GTE_SMALL_TOKENIZER_URL_DEFAULT.to_string())
}
