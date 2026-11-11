// SPDX-License-Identifier: GPL-3.0-only
#![allow(dead_code)]
//! Dynamic modality negotiation and thread-safe per-session [`ModelRegistry`] for connected MCP agents.

use std::collections::HashMap;
use std::fmt;
use std::ops::{BitAnd, BitAndAssign, BitOr, BitOrAssign};
use std::sync::{Arc, OnceLock, RwLock};

use anyhow::{Result, bail};
use regex::Regex;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

/// Strict regex pattern for lowercase `"provider/model"` identifiers.
pub const MODEL_ID_PATTERN: &str = r"^[a-z0-9_-]+/[a-z0-9._:-]+$";

/// Input modalities a connected model natively accepts in tool results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Modality {
    Image,
    Video,
    Audio,
}

impl fmt::Display for Modality {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Modality::Image => f.write_str("image"),
            Modality::Video => f.write_str("video"),
            Modality::Audio => f.write_str("audio"),
        }
    }
}

/// Bitflags representing the set of input modalities supported by an agent session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct Modalities(u8);

impl Modalities {
    pub const NONE: Self = Self(0);
    pub const IMAGE: Self = Self(1 << 0);
    pub const VIDEO: Self = Self(1 << 1);
    pub const AUDIO: Self = Self(1 << 2);
    pub const ALL: Self = Self(Self::IMAGE.0 | Self::VIDEO.0 | Self::AUDIO.0);

    pub const fn empty() -> Self {
        Self::NONE
    }

    pub const fn bits(self) -> u8 {
        self.0
    }

    pub const fn from_bits_truncate(bits: u8) -> Self {
        Self(bits & Self::ALL.0)
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    pub const fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }

    pub const fn remove(&mut self, other: Self) {
        self.0 &= !other.0;
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Convert the set of bitflags back into a list of [`Modality`] values.
    pub fn to_vec(self) -> Vec<Modality> {
        let mut out = Vec::new();
        if self.contains(Self::IMAGE) {
            out.push(Modality::Image);
        }
        if self.contains(Self::VIDEO) {
            out.push(Modality::Video);
        }
        if self.contains(Self::AUDIO) {
            out.push(Modality::Audio);
        }
        out
    }

    /// Resolve the preferred visual output format for multi-frame captures.
    ///
    /// Checks `VIDEO` first:
    /// - `VIDEO` -> [`VisualFormat::Video`]
    /// - `IMAGE` -> [`VisualFormat::ContactSheet`]
    /// - otherwise -> [`VisualFormat::TextSummary`]
    pub fn preferred_visual(&self) -> VisualFormat {
        if self.contains(Self::VIDEO) {
            VisualFormat::Video
        } else if self.contains(Self::IMAGE) {
            VisualFormat::ContactSheet
        } else {
            VisualFormat::TextSummary
        }
    }

    /// Resolve the preferred audio output format for audio captures.
    ///
    /// Checks `AUDIO` first:
    /// - `AUDIO` -> [`AudioFormat::Wav`]
    /// - `IMAGE` -> [`AudioFormat::Spectrogram`]
    /// - otherwise -> [`AudioFormat::MetricsText`]
    pub fn preferred_audio(&self) -> AudioFormat {
        if self.contains(Self::AUDIO) {
            AudioFormat::Wav
        } else if self.contains(Self::IMAGE) {
            AudioFormat::Spectrogram
        } else {
            AudioFormat::MetricsText
        }
    }
}

impl From<Modality> for Modalities {
    fn from(modality: Modality) -> Self {
        match modality {
            Modality::Image => Self::IMAGE,
            Modality::Video => Self::VIDEO,
            Modality::Audio => Self::AUDIO,
        }
    }
}

impl From<&[Modality]> for Modalities {
    fn from(slice: &[Modality]) -> Self {
        let mut flags = Self::NONE;
        for &m in slice {
            flags |= Self::from(m);
        }
        flags
    }
}

impl<const N: usize> From<&[Modality; N]> for Modalities {
    fn from(arr: &[Modality; N]) -> Self {
        Self::from(arr.as_slice())
    }
}

impl<const N: usize> From<[Modality; N]> for Modalities {
    fn from(arr: [Modality; N]) -> Self {
        Self::from(arr.as_slice())
    }
}

impl From<Vec<Modality>> for Modalities {
    fn from(vec: Vec<Modality>) -> Self {
        Self::from(vec.as_slice())
    }
}

impl FromIterator<Modality> for Modalities {
    fn from_iter<T: IntoIterator<Item = Modality>>(iter: T) -> Self {
        let mut flags = Self::NONE;
        for m in iter {
            flags |= Self::from(m);
        }
        flags
    }
}

impl BitOr for Modalities {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for Modalities {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl BitAnd for Modalities {
    type Output = Self;

    fn bitand(self, rhs: Self) -> Self::Output {
        Self(self.0 & rhs.0)
    }
}

impl BitAndAssign for Modalities {
    fn bitand_assign(&mut self, rhs: Self) {
        self.0 &= rhs.0;
    }
}

impl fmt::Display for Modalities {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let items: Vec<String> = self.to_vec().into_iter().map(|m| m.to_string()).collect();
        if items.is_empty() {
            f.write_str("text-only")
        } else {
            f.write_str(&items.join(","))
        }
    }
}

/// Preferred visual representation for multi-frame captures based on [`Modalities`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum VisualFormat {
    Video,
    ContactSheet,
    TextSummary,
}

/// Preferred audio representation for sound captures based on [`Modalities`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AudioFormat {
    Wav,
    Spectrogram,
    MetricsText,
}

/// Arguments accepted by the `ping` tool to register the calling model and its supported modalities.
#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PingArgs {
    /// Strict "provider/model" identifier in lowercase, e.g. "google/gemini-2.5-pro",
    /// "anthropic/claude-sonnet-4", "openai/gpt-4o", "ollama/llava:13b".
    #[schemars(regex(pattern = r"^[a-z0-9_-]+/[a-z0-9._:-]+$"))]
    pub model: Option<String>,
    /// Input modalities this model natively accepts in tool results: "image", "video", "audio".
    pub modalities: Option<Vec<Modality>>,
}

/// Validates a strict lowercase `"provider/model"` identifier against `^[a-z0-9_-]+/[a-z0-9._:-]+$`.
///
/// Returns `(provider, model_name)` if valid.
pub fn validate_model_id(id: &str) -> Result<(&str, &str)> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(MODEL_ID_PATTERN).expect("valid MODEL_ID_PATTERN regex"));
    if !re.is_match(id) {
        bail!(
            "invalid model identifier {id:?}: must be lowercase \"provider/model\" matching {MODEL_ID_PATTERN} \
             (e.g. \"google/gemini-2.5-pro\", \"anthropic/claude-sonnet-4\", \"openai/gpt-4o\", \"ollama/llava:13b\")"
        );
    }
    let (provider, name) = id.split_once('/').expect("regex guarantees '/' separator");
    Ok((provider, name))
}

/// Metadata and negotiated modalities for a connected agent's model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub provider: String,
    pub name: String,
    pub client_app: String,
    pub modalities: Modalities,
}

impl Model {
    /// Validate `id` as `"provider/model"` and construct a [`Model`].
    pub fn new(id: &str, client_app: impl Into<String>, modalities: Modalities) -> Result<Self> {
        let (provider, name) = validate_model_id(id)?;
        Ok(Self {
            id: id.to_string(),
            provider: provider.to_string(),
            name: name.to_string(),
            client_app: client_app.into(),
            modalities,
        })
    }

    /// Resolve a [`Model`] from [`PingArgs`] and the calling client application name.
    ///
    /// When `args.modalities` is omitted, defaults to [`Modalities::IMAGE`].
    /// When `args.model` is omitted, uses `"unknown/unspecified"`.
    pub fn from_ping(args: &PingArgs, client_app: impl Into<String>) -> Result<Self> {
        let modalities = match &args.modalities {
            Some(list) => Modalities::from(list.as_slice()),
            None => Modalities::IMAGE,
        };
        let id = args.model.as_deref().unwrap_or("unknown/unspecified");
        Self::new(id, client_app, modalities)
    }
}

/// Thread-safe session registry mapping each connected agent's session/peer key to its [`Model`].
#[derive(Debug, Clone, Default)]
pub struct ModelRegistry {
    inner: Arc<RwLock<HashMap<String, Model>>>,
}

impl ModelRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Access the underlying `Arc<RwLock<HashMap<String, Model>>>`.
    pub fn inner(&self) -> &Arc<RwLock<HashMap<String, Model>>> {
        &self.inner
    }

    /// Insert or replace the [`Model`] associated with `session_key`.
    pub fn insert(&self, session_key: impl Into<String>, model: Model) -> Option<Model> {
        self.inner.write().unwrap().insert(session_key.into(), model)
    }

    /// Validate `args` and store the resulting [`Model`] under `session_key`.
    pub fn register_ping(
        &self,
        session_key: impl Into<String>,
        args: &PingArgs,
        client_app: impl Into<String>,
    ) -> Result<Model> {
        let model = Model::from_ping(args, client_app)?;
        self.insert(session_key, model.clone());
        Ok(model)
    }

    /// Retrieve a clone of the [`Model`] registered for `session_key`.
    pub fn get(&self, session_key: &str) -> Option<Model> {
        self.inner.read().unwrap().get(session_key).cloned()
    }

    /// Return the registered [`Modalities`] for `session_key`, or [`Modalities::IMAGE`] if unrecorded.
    pub fn modalities_or_default(&self, session_key: &str) -> Modalities {
        self.get(session_key).map_or(Modalities::IMAGE, |m| m.modalities)
    }

    /// Remove and return the [`Model`] for `session_key`.
    pub fn remove(&self, session_key: &str) -> Option<Model> {
        self.inner.write().unwrap().remove(session_key)
    }

    /// Number of active sessions in the registry.
    pub fn len(&self) -> usize {
        self.inner.read().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.read().unwrap().is_empty()
    }

    /// Clone a snapshot of all registered session models.
    pub fn snapshot(&self) -> HashMap<String, Model> {
        self.inner.read().unwrap().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_priority_over_image() {
        let both = Modalities::from(&[Modality::Image, Modality::Video][..]);
        assert!(both.contains(Modalities::IMAGE));
        assert!(both.contains(Modalities::VIDEO));
        assert_eq!(both.preferred_visual(), VisualFormat::Video);

        let all = Modalities::from(&[Modality::Image, Modality::Video, Modality::Audio][..]);
        assert_eq!(all.preferred_visual(), VisualFormat::Video);

        let image_only = Modalities::from(&[Modality::Image][..]);
        assert_eq!(image_only.preferred_visual(), VisualFormat::ContactSheet);

        let audio_only = Modalities::from(&[Modality::Audio][..]);
        assert_eq!(audio_only.preferred_visual(), VisualFormat::TextSummary);

        let empty = Modalities::NONE;
        assert_eq!(empty.preferred_visual(), VisualFormat::TextSummary);
    }

    #[test]
    fn audio_priority_over_image() {
        let both = Modalities::from(&[Modality::Image, Modality::Audio][..]);
        assert!(both.contains(Modalities::IMAGE));
        assert!(both.contains(Modalities::AUDIO));
        assert_eq!(both.preferred_audio(), AudioFormat::Wav);

        let image_only = Modalities::from(&[Modality::Image][..]);
        assert_eq!(image_only.preferred_audio(), AudioFormat::Spectrogram);

        let video_only = Modalities::from(&[Modality::Video][..]);
        assert_eq!(video_only.preferred_audio(), AudioFormat::MetricsText);

        let empty = Modalities::NONE;
        assert_eq!(empty.preferred_audio(), AudioFormat::MetricsText);
    }

    #[test]
    fn provider_model_validation() {
        assert_eq!(
            validate_model_id("google/gemini-2.5-pro").unwrap(),
            ("google", "gemini-2.5-pro")
        );
        assert_eq!(
            validate_model_id("anthropic/claude-sonnet-4").unwrap(),
            ("anthropic", "claude-sonnet-4")
        );
        assert_eq!(
            validate_model_id("openai/gpt-4o").unwrap(),
            ("openai", "gpt-4o")
        );
        assert_eq!(
            validate_model_id("ollama/llava:13b").unwrap(),
            ("ollama", "llava:13b")
        );
        assert_eq!(
            validate_model_id("local_provider-1/model_v1.2:latest").unwrap(),
            ("local_provider-1", "model_v1.2:latest")
        );

        // Invalid cases: missing slash, uppercase, spaces, multiple slashes, empty parts
        assert!(validate_model_id("gemini-2.5-pro").is_err());
        assert!(validate_model_id("Google/gemini-2.5-pro").is_err());
        assert!(validate_model_id("google/Gemini-2.5-Pro").is_err());
        assert!(validate_model_id("google/").is_err());
        assert!(validate_model_id("/gemini-2.5-pro").is_err());
        assert!(validate_model_id("google/gemini/2.5").is_err());
        assert!(validate_model_id("google/gemini 2.5").is_err());
        assert!(validate_model_id("").is_err());
    }

    #[test]
    fn model_registry_lookup_and_insertion() {
        let registry = ModelRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.modalities_or_default("session-a"), Modalities::IMAGE);

        let gemini_ping = PingArgs {
            model: Some("google/gemini-2.5-pro".to_string()),
            modalities: Some(vec![Modality::Image, Modality::Video, Modality::Audio]),
        };
        let claude_ping = PingArgs {
            model: Some("anthropic/claude-sonnet-4".to_string()),
            modalities: Some(vec![Modality::Image]),
        };

        let m1 = registry
            .register_ping("session-a", &gemini_ping, "antigravity")
            .unwrap();
        assert_eq!(m1.provider, "google");
        assert_eq!(m1.name, "gemini-2.5-pro");
        assert_eq!(m1.client_app, "antigravity");
        assert_eq!(m1.modalities.preferred_visual(), VisualFormat::Video);
        assert_eq!(m1.modalities.preferred_audio(), AudioFormat::Wav);

        let m2 = registry
            .register_ping("session-b", &claude_ping, "claude-code")
            .unwrap();
        assert_eq!(m2.provider, "anthropic");
        assert_eq!(m2.name, "claude-sonnet-4");
        assert_eq!(m2.client_app, "claude-code");
        assert_eq!(m2.modalities.preferred_visual(), VisualFormat::ContactSheet);
        assert_eq!(m2.modalities.preferred_audio(), AudioFormat::Spectrogram);

        assert_eq!(registry.len(), 2);
        assert_eq!(registry.get("session-a").unwrap(), m1);
        assert_eq!(registry.get("session-b").unwrap(), m2);
        assert!(registry.get("session-c").is_none());

        let removed = registry.remove("session-a").unwrap();
        assert_eq!(removed, m1);
        assert_eq!(registry.len(), 1);
    }
}
