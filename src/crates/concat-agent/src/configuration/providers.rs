// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah
//! Reviewed public product endpoints. Presence is not an account/transport validation.

use super::ModelProtocol;

/// Explicit discovery adapter; unsupported products never pretend to fetch models.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirectoryProtocol {
    /// A documented OpenAI-compatible list response and optional id cursor.
    OpenAi,
    /// An OpenAI-compatible complete list with no documented cursor protocol.
    OpenAiComplete,
    /// Anthropic's model list and after_id cursor.
    Anthropic,
    /// Google's models resource and page token.
    Google,
    /// Ark returns all account-visible versions in one documented response.
    Ark,
    /// DashScope native API uses numbered pages and an explicit total count.
    DashScope,
    /// Public model names, explicitly not an account directory request.
    Recommended,
}

/// One supported product and region per supplier; subscriptions are separate products.
pub struct ProviderPreset {
    /// Stable supplier identifier, independent of translated labels.
    pub id: &'static str,
    /// Official supplier brand for documentation and diagnostics.
    pub name: &'static str,
    /// Reviewed product region, included in the cache scope.
    pub region: &'static str,
    /// Exact reviewed base address; custom services supply their own.
    pub endpoint: &'static str,
    /// Original adapter protocol used for model inference.
    pub protocol: ModelProtocol,
    /// Reviewed directory response protocol or recommendation fallback.
    pub directory: DirectoryProtocol,
    /// Primary source for this product and its directory behavior.
    pub documentation: &'static str,
    /// Documented identifiers; their presence never proves account access.
    pub recommendations: &'static [&'static str],
}

use DirectoryProtocol::{Anthropic, Ark, DashScope, Google, OpenAi, OpenAiComplete, Recommended};
use ModelProtocol::{AnthropicMessages, GoogleGenerativeAi, OpenAiCompletions, OpenAiResponses};

/// Official pages reviewed 2026-11-23. No static recommendation is an account catalogue.
pub const PROVIDERS: &[ProviderPreset] = &[
    ProviderPreset {
        id: "deepseek",
        name: "DeepSeek",
        region: "global",
        endpoint: "https://api.deepseek.com",
        protocol: OpenAiCompletions,
        directory: OpenAi,
        documentation: "https://api-docs.deepseek.com/api/list-models/",
        recommendations: &[],
    },
    ProviderPreset {
        id: "openai",
        name: "OpenAI",
        region: "global",
        endpoint: "https://api.openai.com/v1",
        protocol: OpenAiResponses,
        directory: OpenAi,
        documentation: "https://developers.openai.com/api/reference/resources/models/methods/list",
        recommendations: &[],
    },
    ProviderPreset {
        id: "anthropic",
        name: "Anthropic Claude",
        region: "global",
        endpoint: "https://api.anthropic.com",
        protocol: AnthropicMessages,
        directory: Anthropic,
        documentation: "https://platform.claude.com/docs/en/api/models/list",
        recommendations: &[],
    },
    ProviderPreset {
        id: "google",
        name: "Google Gemini",
        region: "global",
        endpoint: "https://generativelanguage.googleapis.com/v1beta",
        protocol: GoogleGenerativeAi,
        directory: Google,
        documentation: "https://aistudio.google.com/api-reference/models/list-models",
        recommendations: &[],
    },
    // The compatible inference endpoint has no /models resource. Discovery
    // uses the documented native /api/v1/models on the same regional origin.
    ProviderPreset {
        id: "qwen",
        name: "通义千问",
        region: "cn-beijing",
        endpoint: "https://dashscope.aliyuncs.com/compatible-mode/v1",
        protocol: OpenAiCompletions,
        directory: DashScope,
        documentation: "https://help.aliyun.com/zh/model-studio/list-models",
        recommendations: &[],
    },
    ProviderPreset {
        id: "doubao",
        name: "豆包",
        region: "cn-beijing",
        endpoint: "https://ark.cn-beijing.volces.com/api/v3",
        protocol: OpenAiCompletions,
        directory: Ark,
        documentation: "https://docs.volcengine.com/docs/ark/models-api?lang=zh",
        recommendations: &[],
    },
    ProviderPreset {
        id: "zhipu",
        name: "智谱",
        region: "cn",
        endpoint: "https://open.bigmodel.cn/api/paas/v4",
        protocol: OpenAiCompletions,
        directory: Recommended,
        documentation: "https://docs.bigmodel.cn/cn/guide/develop/openai/introduction",
        recommendations: &["glm-5.3", "glm-5.3-flash"],
    },
    ProviderPreset {
        id: "moonshot",
        name: "月之暗面",
        region: "cn",
        endpoint: "https://api.moonshot.cn/v1",
        protocol: OpenAiCompletions,
        directory: OpenAi,
        documentation: "https://platform.kimi.com/docs/api/list-models",
        recommendations: &[],
    },
    ProviderPreset {
        id: "minimax",
        name: "MiniMax",
        region: "cn",
        endpoint: "https://api.minimax.cn/v1",
        protocol: OpenAiCompletions,
        directory: OpenAiComplete,
        documentation: "https://platform.minimax.cn/docs/api-reference/models/openai/api/list-models.json",
        recommendations: &[],
    },
    ProviderPreset {
        id: "siliconflow",
        name: "硅基流动",
        region: "cn",
        endpoint: "https://api.siliconflow.cn/v1",
        protocol: OpenAiCompletions,
        directory: OpenAi,
        documentation: "https://github.com/siliconflow-inc/siliconflow-api/blob/main/openapi.yaml",
        recommendations: &[],
    },
    ProviderPreset {
        id: "openrouter",
        name: "OpenRouter",
        region: "global",
        endpoint: "https://openrouter.ai/api/v1",
        protocol: OpenAiCompletions,
        directory: OpenAi,
        documentation: "https://openrouter.ai/docs/api/api-reference/models/get-models",
        recommendations: &[],
    },
    ProviderPreset {
        id: "custom",
        name: "Custom",
        region: "custom",
        endpoint: "",
        protocol: OpenAiCompletions,
        directory: OpenAi,
        documentation: "",
        recommendations: &[],
    },
];

/// Resolve one reviewed supplier by stable identity.
pub fn preset(id: &str) -> Option<&'static ProviderPreset> {
    PROVIDERS.iter().find(|p| p.id == id)
}

/// Choose the reviewed protocol, retaining explicit custom-service selection.
pub fn directory_protocol(provider: &str, protocol: ModelProtocol) -> DirectoryProtocol {
    if provider == "custom" {
        match protocol {
            AnthropicMessages => Anthropic,
            GoogleGenerativeAi => Google,
            _ => OpenAi,
        }
    } else {
        preset(provider).map_or(Recommended, |p| p.directory)
    }
}
