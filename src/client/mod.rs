#[macro_use]
mod common;
mod message;
mod model;
mod prompt_format;
mod reply_handler;

pub use common::*;
pub use message::*;
pub use model::*;
pub use prompt_format::*;
pub use reply_handler::*;

register_client!(
    (openai, "openai", OpenAIConfig, OpenAIClient),
    (gemini, "gemini", GeminiConfig, GeminiClient),
    (claude, "claude", ClaudeConfig, ClaudeClient),
    (mistral, "mistral", MistralConfig, MistralClient),
    (cohere, "cohere", CohereConfig, CohereClient),
    (perplexity, "perplexity", PerplexityConfig, PerplexityClient),
    (groq, "groq", GroqConfig, GroqClient),
    (
        openai_compatible,
        "openai-compatible",
        OpenAICompatibleConfig,
        OpenAICompatibleClient
    ),
    (ollama, "ollama", OllamaConfig, OllamaClient),
    (
        azure_openai,
        "azure-openai",
        AzureOpenAIConfig,
        AzureOpenAIClient
    ),
    (bedrock, "bedrock", BedrockConfig, BedrockClient),
    (vertexai, "vertexai", VertexAIConfig, VertexAIClient),
    (ernie, "ernie", ErnieConfig, ErnieClient),
    (qianwen, "qianwen", QianwenConfig, QianwenClient),
    (moonshot, "moonshot", MoonshotConfig, MoonshotClient),
);
