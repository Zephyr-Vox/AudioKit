//! Shared microphone processing settings and accepted defaults.

/// Selects the background-noise suppression aggressiveness.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoiseSuppressionMode {
    /// Disable suppression, useful as a comparison baseline.
    Off,
    /// Light suppression with the least speech coloration.
    Low,
    /// Balanced suppression used by the voice client by default.
    #[default]
    Moderate,
    /// Stronger suppression with greater risk of speech artifacts.
    High,
    /// Maximum available suppression, with the highest speech-distortion risk.
    VeryHigh,
}

/// Switches and level for the microphone processing stages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioProcessingOptions {
    /// Enables the low-frequency rumble high-pass filter.
    pub high_pass_filter: bool,
    /// Noise suppression level; `Off` disables the Sonora suppression module.
    pub noise_suppression: NoiseSuppressionMode,
    /// Enables Sonora AGC2, including its peak-protection limiter.
    pub gain_controller2: bool,
    /// Enables Sonora AGC2 adaptive digital gain with the library's defaults.
    #[serde(default)]
    pub adaptive_gain: bool,
}

impl Default for AudioProcessingOptions {
    fn default() -> Self {
        Self {
            high_pass_filter: true,
            noise_suppression: NoiseSuppressionMode::default(),
            gain_controller2: true,
            adaptive_gain: false,
        }
    }
}
