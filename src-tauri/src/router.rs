use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::db;
use crate::models::{AiProvider, RouteDecision};
use crate::providers::DEFAULT_OLLAMA_URL;

/// Smart router that distributes images across available AI providers
/// to minimize cost while maximizing throughput.
///
/// Strategy:
/// 1. Only consider providers that have a valid API key configured
/// 2. Sort by cost_per_image ascending (cheapest first)
/// 3. Spread load: round-robin among the cheapest tier if costs are close
/// 4. On failure, automatically failover to next cheapest provider
pub struct SmartRouter {
    keys: HashMap<AiProvider, String>,
    models: HashMap<AiProvider, String>,
    /// Tracks how many images each provider has been assigned this session
    counters: HashMap<AiProvider, usize>,
    /// Tracks consecutive errors per provider (for circuit-breaking)
    error_streak: HashMap<AiProvider, usize>,
    /// Max consecutive errors before temporarily disabling
    max_errors: usize,
}

impl SmartRouter {
    pub fn new(db_conn: &Arc<Mutex<rusqlite::Connection>>) -> Self {
        let conn = db_conn.lock().unwrap();
        let mut keys = HashMap::new();
        let mut models = HashMap::new();

        for provider in AiProvider::all() {
            let model_setting = format!("model_{}", provider.key_name());
            let model = db::get_setting(&conn, &model_setting)
                .ok()
                .flatten()
                .unwrap_or_else(|| provider.default_model().to_string());
            models.insert(*provider, model);

            if *provider == AiProvider::Local {
                // Local provider: use ollama_endpoint as the "key" field.
                // Mark it available only if enabled explicitly (user toggled it on).
                let enabled = db::get_setting(&conn, "enabled_local")
                    .ok()
                    .flatten()
                    .map(|v| v == "true" || v == "1")
                    .unwrap_or(false);
                if enabled {
                    let endpoint = db::get_setting(&conn, "ollama_endpoint")
                        .ok()
                        .flatten()
                        .unwrap_or_else(|| DEFAULT_OLLAMA_URL.to_string());
                    keys.insert(*provider, endpoint);
                }
            } else {
                let key_setting = format!("api_key_{}", provider.key_name());
                if let Ok(Some(key)) = db::get_setting(&conn, &key_setting) {
                    if !key.is_empty() {
                        keys.insert(*provider, key);
                    }
                }
            }
        }

        SmartRouter {
            keys,
            models,
            counters: HashMap::new(),
            error_streak: HashMap::new(),
            max_errors: 5,
        }
    }

    /// Returns the list of providers that have keys and haven't been circuit-broken
    pub fn available_providers(&self) -> Vec<AiProvider> {
        let mut providers: Vec<AiProvider> = self
            .keys
            .keys()
            .filter(|p| {
                self.error_streak.get(p).copied().unwrap_or(0) < self.max_errors
            })
            .copied()
            .collect();

        // Sort by cost ascending
        providers.sort_by(|a, b| {
            a.cost_per_image()
                .partial_cmp(&b.cost_per_image())
                .unwrap()
        });

        providers
    }

    /// Pick the next best provider for an image
    pub fn next_route(&mut self) -> Option<RouteDecision> {
        let providers = self.available_providers();
        if providers.is_empty() {
            return None;
        }

        // Among available, find cheapest tier (within 50% of cheapest)
        let cheapest_cost = providers[0].cost_per_image();
        let tier: Vec<&AiProvider> = providers
            .iter()
            .filter(|p| p.cost_per_image() <= cheapest_cost * 1.5)
            .collect();

        // Round-robin within tier: pick the one with fewest images assigned
        let best = tier
            .iter()
            .min_by_key(|p| self.counters.get(p).copied().unwrap_or(0))
            .unwrap();

        let provider = **best;
        *self.counters.entry(provider).or_insert(0) += 1;

        Some(RouteDecision {
            provider,
            api_key: self.keys[&provider].clone(),
            model: self.models.get(&provider).cloned().unwrap_or_else(|| {
                provider.default_model().to_string()
            }),
        })
    }

    /// Get a fallback route after a failure (exclude the failed provider)
    pub fn fallback_route(&mut self, failed: AiProvider) -> Option<RouteDecision> {
        let streak = self.error_streak.entry(failed).or_insert(0);
        *streak += 1;

        let providers = self.available_providers();
        let fallback = providers.into_iter().find(|p| *p != failed)?;

        *self.counters.entry(fallback).or_insert(0) += 1;
        Some(RouteDecision {
            provider: fallback,
            api_key: self.keys[&fallback].clone(),
            model: self.models.get(&fallback).cloned().unwrap_or_else(|| {
                fallback.default_model().to_string()
            }),
        })
    }

    /// Report success — resets error streak
    pub fn report_success(&mut self, provider: AiProvider) {
        self.error_streak.insert(provider, 0);
    }

    /// Permanently disable a provider this session (e.g. invalid API key)
    pub fn disable_provider(&mut self, provider: AiProvider) {
        self.error_streak.insert(provider, self.max_errors + 1);
        self.keys.remove(&provider);
    }

    /// Check how many providers are available
    pub fn provider_count(&self) -> usize {
        self.available_providers().len()
    }

    /// Check if the only active provider is local Ollama
    pub fn is_local_only(&self) -> bool {
        let providers = self.available_providers();
        providers.len() == 1 && providers[0] == AiProvider::Local
    }

    /// Get the cheapest available provider for text-only tasks (translation)
    pub fn cheapest_text_provider(&self) -> Option<(AiProvider, String)> {
        let providers = self.available_providers();
        // Local is free; then Gemini, OpenAI, Claude, Grok
        let preference = [
            AiProvider::Local,
            AiProvider::Gemini,
            AiProvider::OpenAI,
            AiProvider::Claude,
            AiProvider::Grok,
        ];
        for p in &preference {
            if providers.contains(p) {
                return Some((*p, self.keys[p].clone()));
            }
        }
        None
    }
}
