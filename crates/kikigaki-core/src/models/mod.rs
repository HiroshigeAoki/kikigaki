use std::path::{Path, PathBuf};

use crate::config::{AsrModel, Capabilities, EngineKind};

/// Model download abstraction.
pub mod fetch;
/// Model installation and verification routines.
pub mod install;

pub use install::{ensure_installed, install, invalidate, InstallReport, Progress, ProgressFn};

/// Version written at the beginning of every successful installation marker.
pub const MANIFEST_VERSION: u32 = 1;

/// Immutable upstream location for a model payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// An asset attached to a sherpa-onnx GitHub release.
    GithubRelease {
        /// Release tag containing the asset.
        tag: &'static str,
        /// Exact release asset name.
        asset: &'static str,
    },
    /// Files stored in a pinned Hugging Face repository revision.
    HuggingFace {
        /// Repository name including its owner.
        repo: &'static str,
        /// Immutable commit revision.
        revision: &'static str,
    },
}

impl Source {
    /// Builds the download URL for a manifest file.
    pub fn url(&self, file_name: &str) -> String {
        match self {
            Self::GithubRelease { tag, asset } => {
                format!("https://github.com/k2-fsa/sherpa-onnx/releases/download/{tag}/{asset}")
            }
            Self::HuggingFace { repo, revision } => {
                format!("https://huggingface.co/{repo}/resolve/{revision}/{file_name}")
            }
        }
    }
}

/// One installed file and its integrity metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManifestFile {
    /// Bare file name used in the model directory.
    pub name: &'static str,
    /// Exact expected byte length.
    pub size: u64,
    /// Lowercase hexadecimal SHA-256 digest.
    pub sha256: &'static str,
}

/// Download and extraction shape for a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Payload {
    /// One directly downloaded file.
    File(ManifestFile),
    /// Multiple files downloaded independently.
    Files(&'static [ManifestFile]),
    /// A bzip2-compressed tar archive containing selected files.
    TarBz2 {
        /// Exact archive byte length.
        archive_size: u64,
        /// Lowercase hexadecimal SHA-256 digest of the archive.
        archive_sha256: &'static str,
        /// Files extracted from the archive.
        files: &'static [ManifestFile],
    },
}

/// Feature that requires a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Requirement {
    /// The selected local ASR transducer.
    Asr(AsrModel),
    /// Voice activity detection for the local engine.
    Vad,
    /// Local punctuation inference.
    Punct,
}

/// One independently installed model package.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Model {
    /// Stable directory and marker identifier.
    pub id: &'static str,
    /// Immutable upstream source.
    pub source: Source,
    /// Download and extraction shape.
    pub payload: Payload,
    /// Runtime capability that consumes this model.
    pub required_for: Requirement,
}

impl Model {
    pub(crate) fn files(&self) -> &[ManifestFile] {
        match &self.payload {
            Payload::File(file) => std::slice::from_ref(file),
            Payload::Files(files) | Payload::TarBz2 { files, .. } => files,
        }
    }
}

const REAZONSPEECH_FILES: &[ManifestFile] = &[
    ManifestFile {
        name: "encoder-epoch-35-avg-1.int8.onnx",
        size: 70_876_409,
        sha256: "ead1579e118b821a767242a8eb9272634b0e63ba16f8dfc4d126732406eae268",
    },
    ManifestFile {
        name: "decoder-epoch-35-avg-1.int8.onnx",
        size: 1_308_690,
        sha256: "d0179db78a2e65445c5c3dc41e94c62068fc539fe4e45060e32f438cca76432f",
    },
    ManifestFile {
        name: "joiner-epoch-35-avg-1.int8.onnx",
        size: 1_033_417,
        sha256: "c7f4ba40a8ae307a6c30b5c06e2570add04466bcb45bab62699f0ec5d00ed495",
    },
    ManifestFile {
        name: "tokens.txt",
        size: 26_631,
        sha256: "144f8a4f639373a1bdf7eabb2437482ef64b0cc5db24ad27cce65f293e4faa24",
    },
];

const HAAS_FILES: &[ManifestFile] = &[
    ManifestFile {
        name: "encoder.quantized.onnx",
        size: 42261252,
        sha256: "aba291d61d3d594bc6fb8c7f90acbd980e7bc7bf6b7caaf3965ddc25e6f42fac",
    },
    ManifestFile {
        name: "encoder.data",
        size: 410744064,
        sha256: "30befbf6aeb75383e6551f5e58de53efdb76972ddc7d0f76070c517899873586",
    },
    ManifestFile {
        name: "decoder.onnx",
        size: 15661602,
        sha256: "2947cb5512699cbba753fe45fe221428aadaa9856597debbb7aabe19cc253622",
    },
    ManifestFile {
        name: "joiner.onnx",
        size: 12158641,
        sha256: "8a68320e750890ff9e944a794b41ae1c42c6f439ac4c273503458a6b42adc12d",
    },
    ManifestFile {
        name: "tokens.txt",
        size: 28704,
        sha256: "ec78b13c5d9f156cf0d5fdbd6b5aec5785680f319c92630c11d5d11b14b83a84",
    },
];

const PUNCT_FILES: &[ManifestFile] = &[
    ManifestFile {
        name: "punct_bert.int8.onnx",
        size: 109_150_947,
        sha256: "0e0e16da171bd7b6e8b0b64734263150a7d0a1b9907864837fb1647ce52e880e",
    },
    ManifestFile {
        name: "vocab.txt",
        size: 27_928,
        sha256: "57411bcac5e9559f2aa4d316a2217289048cb40fe23187b02a81aeb3e5d61cf3",
    },
];

/// Stable model-package identifier for the ReazonSpeech transducer.
pub const ASR_MODEL_ID: &str = "reazonspeech-ja-en-2025-01-17";
/// Stable model-package identifier for Haas.
pub const HAAS_MODEL_ID: &str = "haas-ja-v1.0-onnx-quantized";
/// Stable model-package identifier for Silero VAD.
pub const VAD_MODEL_ID: &str = "silero-vad";

/// Complete pinned model manifest for this build target.
pub const MODELS: &[Model] = &[
    Model {
        id: ASR_MODEL_ID,
        source: Source::GithubRelease {
            tag: "asr-models",
            asset: "sherpa-onnx-zipformer-ja-en-reazonspeech-2025-01-17.tar.bz2",
        },
        payload: Payload::TarBz2 {
            archive_size: 437_969_761,
            archive_sha256: "dc03758608c0280e2cbcaac4597467ffcf846ae0b06436f1706738a11da86f5d",
            files: REAZONSPEECH_FILES,
        },
        required_for: Requirement::Asr(AsrModel::ReazonSpeech),
    },
    Model {
        id: VAD_MODEL_ID,
        source: Source::GithubRelease {
            tag: "asr-models",
            asset: "silero_vad.onnx",
        },
        payload: Payload::File(ManifestFile {
            name: "silero_vad.onnx",
            size: 643_854,
            sha256: "9e2449e1087496d8d4caba907f23e0bd3f78d91fa552479bb9c23ac09cbb1fd6",
        }),
        required_for: Requirement::Vad,
    },
    Model {
        id: "mojicast-punct",
        source: Source::HuggingFace {
            repo: "ishiki-emo/mojicast-punct-onnx",
            revision: "6bef44545db904999043648af48ee17cd6177ee4",
        },
        payload: Payload::Files(PUNCT_FILES),
        required_for: Requirement::Punct,
    },
    Model {
        id: HAAS_MODEL_ID,
        source: Source::HuggingFace {
            repo: "oboroge0/Haas-ja-v1.0-onnx-quantized",
            revision: "7f5ab0a4b1bf39db78afd6691eeecbc4f9d42e39",
        },
        payload: Payload::Files(HAAS_FILES),
        required_for: Requirement::Asr(AsrModel::Haas),
    },
];

/// Selects models required by an engine and effective local capabilities.
pub fn required(
    kind: EngineKind,
    asr_model: AsrModel,
    caps: Capabilities,
    punct_effective: bool,
) -> Vec<&'static Model> {
    MODELS
        .iter()
        .filter(|model| match model.required_for {
            Requirement::Asr(model) => kind == EngineKind::Local && model == asr_model,
            Requirement::Vad => kind == EngineKind::Local,
            Requirement::Punct => caps.punct && punct_effective,
        })
        .collect()
}

/// Returns the installation directory for a model identifier.
pub fn model_dir(models_dir: &Path, id: &str) -> PathBuf {
    models_dir.join(id)
}

/// Returns the deterministic successful-install marker contents for a model.
pub fn ok_marker(model: &Model) -> String {
    let mut files = model.files().iter().collect::<Vec<_>>();
    files.sort_unstable_by_key(|file| file.name);
    let mut marker = format!("v{MANIFEST_VERSION}\n");
    for file in files {
        marker.push_str(file.name);
        marker.push(' ');
        marker.push_str(file.sha256);
        marker.push('\n');
    }
    marker
}

/// Checks the marker and exact file sizes for an installed model.
pub fn is_installed(models_dir: &Path, model: &Model) -> bool {
    let dir = model_dir(models_dir, model.id);
    std::fs::read_to_string(dir.join(".ok")).is_ok_and(|contents| contents == ok_marker(model))
        && model.files().iter().all(|file| {
            std::fs::metadata(dir.join(file.name))
                .is_ok_and(|metadata| metadata.is_file() && metadata.len() == file.size)
        })
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn haas_manifest_matches_pinned_files() {
        let model = MODELS.iter().find(|m| m.id == HAAS_MODEL_ID).unwrap();
        assert_eq!(model.required_for, Requirement::Asr(AsrModel::Haas));
        assert_eq!(
            model.source,
            Source::HuggingFace {
                repo: "oboroge0/Haas-ja-v1.0-onnx-quantized",
                revision: "7f5ab0a4b1bf39db78afd6691eeecbc4f9d42e39",
            }
        );
        assert!(matches!(model.payload, Payload::Files(_)));
        assert_eq!(
            model
                .files()
                .iter()
                .map(|f| (f.name, f.size, f.sha256))
                .collect::<Vec<_>>(),
            [
                (
                    "encoder.quantized.onnx",
                    42261252,
                    "aba291d61d3d594bc6fb8c7f90acbd980e7bc7bf6b7caaf3965ddc25e6f42fac"
                ),
                (
                    "encoder.data",
                    410744064,
                    "30befbf6aeb75383e6551f5e58de53efdb76972ddc7d0f76070c517899873586"
                ),
                (
                    "decoder.onnx",
                    15661602,
                    "2947cb5512699cbba753fe45fe221428aadaa9856597debbb7aabe19cc253622"
                ),
                (
                    "joiner.onnx",
                    12158641,
                    "8a68320e750890ff9e944a794b41ae1c42c6f439ac4c273503458a6b42adc12d"
                ),
                (
                    "tokens.txt",
                    28704,
                    "ec78b13c5d9f156cf0d5fdbd6b5aec5785680f319c92630c11d5d11b14b83a84"
                )
            ]
        );
    }

    #[test]
    fn required_models_select_only_the_applied_asr() {
        let caps = Capabilities {
            punct: true,
            remote_engine: true,
        };
        let mut config = crate::config::Config::default();
        config.remote.server_punctuates = false;
        for model in [AsrModel::ReazonSpeech, AsrModel::Haas] {
            for enabled in [false, true] {
                config.punct.enabled = if enabled {
                    crate::config::PunctEnabled::On
                } else {
                    crate::config::PunctEnabled::Off
                };
                for engine in [EngineKind::Local, EngineKind::Remote] {
                    let punct = crate::config::punct_effective(&config, caps, engine, model);
                    let mut ids = required(engine, model, caps, punct)
                        .iter()
                        .map(|m| m.id)
                        .collect::<Vec<_>>();
                    let mut expected = match engine {
                        EngineKind::Local => vec![
                            if model == AsrModel::Haas {
                                HAAS_MODEL_ID
                            } else {
                                ASR_MODEL_ID
                            },
                            VAD_MODEL_ID,
                        ],
                        EngineKind::Remote => vec![],
                    };
                    if enabled && !(engine == EngineKind::Local && model == AsrModel::Haas) {
                        expected.push("mojicast-punct");
                    }
                    ids.sort_unstable();
                    expected.sort_unstable();
                    assert_eq!(ids, expected);
                }
            }
        }
    }

    #[test]
    fn manifest_entries_are_well_formed() {
        let mut ids = HashSet::new();
        for model in MODELS {
            assert_ne!(model.id, "hotwords", "model id is reserved: hotwords");
            assert_ne!(model.id, "ort", "model id is reserved: ort");
            assert!(ids.insert(model.id), "duplicate model id: {}", model.id);
            for file in model.files() {
                assert!(!file.name.is_empty());
                assert_eq!(
                    std::path::Path::new(file.name).file_name().unwrap(),
                    file.name
                );
                assert!(!file.name.contains('/'));
                assert!(!file.name.contains(".."));
                assert_eq!(file.sha256.len(), 64);
                assert!(file
                    .sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)));
                assert!(!model.source.url(file.name).is_empty());
            }
            if let Payload::TarBz2 { archive_sha256, .. } = model.payload {
                assert_eq!(archive_sha256.len(), 64);
                assert!(archive_sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)));
            }
        }
    }

    #[test]
    fn source_urls_are_pinned_and_exact() {
        let silero = MODELS
            .iter()
            .find(|model| model.id == "silero-vad")
            .unwrap();
        assert_eq!(
            silero.source.url("silero_vad.onnx"),
            "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx"
        );
        let punct = MODELS
            .iter()
            .find(|model| model.id == "mojicast-punct")
            .unwrap();
        assert_eq!(
            punct.source.url("vocab.txt"),
            "https://huggingface.co/ishiki-emo/mojicast-punct-onnx/resolve/6bef44545db904999043648af48ee17cd6177ee4/vocab.txt"
        );
    }

    #[test]
    fn required_models_follow_engine_and_punctuation() {
        assert!(required(
            EngineKind::Remote,
            AsrModel::ReazonSpeech,
            Capabilities {
                punct: false,
                remote_engine: true,
            },
            false
        )
        .is_empty());
        let remote_punct = required(
            EngineKind::Remote,
            AsrModel::ReazonSpeech,
            Capabilities {
                punct: true,
                remote_engine: true,
            },
            true,
        );
        assert_eq!(
            remote_punct
                .iter()
                .map(|model| model.id)
                .collect::<Vec<_>>(),
            ["mojicast-punct"]
        );
        let local = required(
            EngineKind::Local,
            AsrModel::ReazonSpeech,
            Capabilities {
                punct: false,
                remote_engine: true,
            },
            false,
        );
        assert_eq!(
            local.iter().map(|model| model.id).collect::<Vec<_>>(),
            ["reazonspeech-ja-en-2025-01-17", "silero-vad"]
        );
    }
}
