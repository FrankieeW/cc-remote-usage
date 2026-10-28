//! Cost Calculator - 计算 API 请求成本
//!
//! 使用高精度 Decimal 类型避免浮点数精度问题

use super::parser::TokenUsage;
use rust_decimal::Decimal;
use std::str::FromStr;

/// 成本明细
#[derive(Debug, Clone)]
pub struct CostBreakdown {
    pub input_cost: Decimal,
    pub output_cost: Decimal,
    pub cache_read_cost: Decimal,
    pub cache_creation_cost: Decimal,
    pub total_cost: Decimal,
}

/// 模型定价信息
#[derive(Debug, Clone)]
pub struct ModelPricing {
    pub input_cost_per_million: Decimal,
    pub output_cost_per_million: Decimal,
    pub cache_read_cost_per_million: Decimal,
    pub cache_creation_cost_per_million: Decimal,
}

/// 成本计算器
pub struct CostCalculator;

impl CostCalculator {
    /// 计算请求成本
    ///
    /// # 参数
    /// - `usage`: Token 使用量
    /// - `pricing`: 模型定价
    /// - `cost_multiplier`: 成本倍数 (provider 自定义)
    ///
    /// # 计算逻辑
    /// - input_cost: input_tokens × 输入价格
    /// - cache_read_cost: cache_read_tokens × 缓存读取价格
    /// - Claude/Anthropic 的 input_tokens 已经不包含 cache_read_tokens
    /// - total_cost: 各项成本之和 × 倍率（倍率只作用于最终总价）
    pub fn calculate(
        usage: &TokenUsage,
        pricing: &ModelPricing,
        cost_multiplier: Decimal,
    ) -> CostBreakdown {
        Self::calculate_with_cache_semantics(usage, pricing, cost_multiplier, false)
    }

    /// 按 app_type 选择输入 token 语义后计算成本。
    ///
    /// Codex/OpenAI Responses 与 Gemini 的输入 token 字段包含 cache read 部分；
    /// Claude/Anthropic 的 input_tokens 已经是 fresh input。
    pub fn calculate_for_app(
        app_type: &str,
        usage: &TokenUsage,
        pricing: &ModelPricing,
        cost_multiplier: Decimal,
    ) -> CostBreakdown {
        let input_includes_cache_read =
            crate::services::sql_helpers::is_cache_inclusive_app(app_type);
        Self::calculate_with_cache_semantics(
            usage,
            pricing,
            cost_multiplier,
            input_includes_cache_read,
        )
    }

    fn calculate_with_cache_semantics(
        usage: &TokenUsage,
        pricing: &ModelPricing,
        cost_multiplier: Decimal,
        input_includes_cache_read: bool,
    ) -> CostBreakdown {
        let million = Decimal::from(1_000_000);

        // OpenAI/Gemini 风格的 input_tokens 包含缓存读取和写入，需要扣除后再按输入价计费；
        // Claude/Anthropic 风格的 input_tokens 已经是 fresh input，不能再次扣减。
        let billable_input_tokens = if input_includes_cache_read {
            usage
                .input_tokens
                .saturating_sub(usage.cache_read_tokens)
                .saturating_sub(usage.cache_creation_tokens)
        } else {
            usage.input_tokens
        };

        // 各项基础成本（不含倍率）
        let input_cost =
            Decimal::from(billable_input_tokens) * pricing.input_cost_per_million / million;
        let output_cost =
            Decimal::from(usage.output_tokens) * pricing.output_cost_per_million / million;
        let cache_read_cost =
            Decimal::from(usage.cache_read_tokens) * pricing.cache_read_cost_per_million / million;
        // Anthropic 5 分钟缓存写入为输入价 1.25 倍、1 小时写入为 2 倍；定价表的
        // cache_creation 单价对应 5 分钟写入，1 小时写入按其 1.6 倍计。
        let cache_creation_1h_tokens = usage
            .cache_creation_1h_tokens
            .min(usage.cache_creation_tokens);
        let cache_creation_cost =
            (Decimal::from(usage.cache_creation_tokens - cache_creation_1h_tokens)
                + Decimal::from(cache_creation_1h_tokens) * Decimal::new(16, 1))
                * pricing.cache_creation_cost_per_million
                / million;

        // 总成本 = 各项基础成本之和 × 倍率
        let base_total = input_cost + output_cost + cache_read_cost + cache_creation_cost;
        let total_cost = base_total * cost_multiplier;

        CostBreakdown {
            input_cost,
            output_cost,
            cache_read_cost,
            cache_creation_cost,
            total_cost,
        }
    }

    pub fn try_calculate_for_app(
        app_type: &str,
        usage: &TokenUsage,
        pricing: Option<&ModelPricing>,
        cost_multiplier: Decimal,
    ) -> Option<CostBreakdown> {
        pricing.map(|p| Self::calculate_for_app(app_type, usage, p, cost_multiplier))
    }
}

impl ModelPricing {
    /// 从字符串创建定价信息
    pub fn from_strings(
        input: &str,
        output: &str,
        cache_read: &str,
        cache_creation: &str,
    ) -> Result<Self, rust_decimal::Error> {
        Ok(Self {
            input_cost_per_million: Decimal::from_str(input)?,
            output_cost_per_million: Decimal::from_str(output)?,
            cache_read_cost_per_million: Decimal::from_str(cache_read)?,
            cache_creation_cost_per_million: Decimal::from_str(cache_creation)?,
        })
    }
}
