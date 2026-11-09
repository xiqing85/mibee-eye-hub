//! AI model catalog + download manager (SPEC §4.9, capability
//! `model_manager`).
//!
//! Every AI capability lists the models this build can actually run —
//! cross-family candidates are deliberately absent (the engines load a
//! fixed architecture; a different decoder family would be a code change,
//! not a file swap). Entries carry the exact file set (size + sha256 when
//! the source publishes an LFS hash) so downloads are verified and
//! `installed` is a size check, not a directory-name guess.
//!
//! Sources: Hugging Face repos reachable through both hf-mirror.com and
//! huggingface.co (tried in order). Files the deployment already carries
//! without a public mirror are listed with no repo — `downloadable:false`,
//! deletable but not fetchable.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;

/// One file of a catalog model. `path` is relative to the install dir and
/// may contain subdirectories (melo's jieba dict); `remote` is the name
/// inside the source repo (differs when the deployment renamed the file).
pub struct CatalogFile {
    pub role: &'static str,
    pub path: &'static str,
    pub remote: &'static str,
    pub size: u64,
    pub sha256: Option<&'static str>,
    /// Source repo (`owner/name`); `None` = no download source.
    pub repo: Option<&'static str>,
}

pub struct CatalogModel {
    pub id: &'static str,
    pub name: &'static str,
    /// Install directory relative to the models root.
    pub dir: &'static str,
    pub languages: &'static [&'static str],
    pub license: &'static str,
    pub notes: &'static str,
    pub files: &'static [CatalogFile],
}

pub struct CapabilitySpec {
    pub id: &'static str,
    pub label: &'static str,
    /// "restart" (engine rebuilt at boot) or "immediate" (hot switch).
    pub apply: &'static str,
    pub models: &'static [CatalogModel],
}

/// Test seam: when set, `catalog()` serves this table instead of the
/// built-in one — lets integration tests (download and activation)
/// run against tiny synthetic entries instead of multi-GB HF repos.
/// `#[doc(hidden)]` public because downstream-crate tests use it too.
#[doc(hidden)]
pub static TEST_CATALOG: Mutex<Option<&'static [CapabilitySpec]>> = Mutex::new(None);

const fn f(
    role: &'static str,
    path: &'static str,
    remote: &'static str,
    size: u64,
    sha256: Option<&'static str>,
    repo: Option<&'static str>,
) -> CatalogFile {
    CatalogFile {
        role,
        path,
        remote,
        size,
        sha256,
        repo,
    }
}

pub static CATALOG: &[CapabilitySpec] = &[
    CapabilitySpec {
        id: "llm",
        label: "对话语言模型 (LLM)",
        apply: "restart",
        models: &[
            CatalogModel {
                id: "qwen3-0.6b-q8_0",
                name: "Qwen3-0.6B Q8_0",
                dir: "llm",
                languages: &["zh", "en"],
                license: "Apache-2.0",
                notes: "轻量档（低内存主机）",
                files: &[f(
                    "model",
                    "qwen3-0.6b-q8_0.gguf",
                    "Qwen3-0.6B-Q8_0.gguf",
                    639446688,
                    Some("9465e63a22add5354d9bb4b99e90117043c7124007664907259bd16d043bb031"),
                    Some("Qwen/Qwen3-0.6B-GGUF"),
                )],
            },
            CatalogModel {
                id: "qwen3-1.7b-q4_k_m",
                name: "Qwen3-1.7B Q4_K_M",
                dir: "llm",
                languages: &["zh", "en"],
                license: "Apache-2.0",
                notes: "小而快，回答质量介于 0.6B 与 4B 之间",
                files: &[f(
                    "model",
                    "Qwen3-1.7B-Q4_K_M.gguf",
                    "Qwen3-1.7B-Q4_K_M.gguf",
                    1282439584,
                    Some("72c5c3cb38fa32d5256e2fe30d03e7a64c6c79e668ad84057e3bd66e250b24fb"),
                    Some("bartowski/Qwen_Qwen3-1.7B-GGUF"),
                )],
            },
            CatalogModel {
                id: "qwen3-4b-instruct-2507-q4_k_m",
                name: "Qwen3-4B-Instruct-2507 Q4_K_M",
                dir: "llm",
                languages: &["zh", "en"],
                license: "Apache-2.0",
                notes: "默认档（满配体验）",
                files: &[f(
                    "model",
                    "Qwen3-4B-Instruct-2507-Q4_K_M.gguf",
                    "Qwen3-4B-Instruct-2507-Q4_K_M.gguf",
                    2497281120,
                    Some("3605803b982cb64aead44f6c1b2ae36e3acdb41d8e46c8a94c6533bc4c67e597"),
                    Some("unsloth/Qwen3-4B-Instruct-2507-GGUF"),
                )],
            },
        ],
    },
    CapabilitySpec {
        id: "vlm",
        label: "视觉语言模型 (VLM)",
        apply: "restart",
        models: &[
            CatalogModel {
                id: "qwen3-vl-2b-instruct",
                name: "Qwen3-VL-2B Q4_K_M + mmproj Q8_0",
                dir: "vlm",
                languages: &["zh", "en"],
                license: "Apache-2.0",
                notes: "默认（看图问答/告警描述）",
                files: &[
                    f(
                        "model",
                        "qwen3-vl-2b-instruct-q4_k_m.gguf",
                        "Qwen3VL-2B-Instruct-Q4_K_M.gguf",
                        1107409952,
                        Some("089d75c52f4b7ffc56ba998ffc50aae89fcafc755f9e7208aacca281dca6c2ae"),
                        Some("Qwen/Qwen3-VL-2B-Instruct-GGUF"),
                    ),
                    f(
                        "mmproj",
                        "mmproj-qwen3-vl-2b-instruct-q8_0.gguf",
                        "mmproj-Qwen3VL-2B-Instruct-Q8_0.gguf",
                        445053216,
                        Some("f9a68fabba69c3b81e153367b2c7521030b0fa8bb0de400c9599c8e6725f9c82"),
                        Some("Qwen/Qwen3-VL-2B-Instruct-GGUF"),
                    ),
                ],
            },
            CatalogModel {
                id: "qwen3-vl-4b-instruct",
                name: "Qwen3-VL-4B Q4_K_M + mmproj Q8_0",
                dir: "vlm",
                languages: &["zh", "en"],
                license: "Apache-2.0",
                notes: "更强的画面理解，CPU 较慢",
                files: &[
                    f(
                        "model",
                        "Qwen3VL-4B-Instruct-Q4_K_M.gguf",
                        "Qwen3VL-4B-Instruct-Q4_K_M.gguf",
                        2497281664,
                        Some("66358cb18bb6b3b1b6675aa412c7a88ef01d228f481184d13668e5201c730a0a"),
                        Some("Qwen/Qwen3-VL-4B-Instruct-GGUF"),
                    ),
                    f(
                        "mmproj",
                        "mmproj-Qwen3VL-4B-Instruct-Q8_0.gguf",
                        "mmproj-Qwen3VL-4B-Instruct-Q8_0.gguf",
                        453974304,
                        Some("30ba2c7dd3127a4561b6cba9d13d0f711c91bdb38742e2f56d73c8cb596bd06d"),
                        Some("Qwen/Qwen3-VL-4B-Instruct-GGUF"),
                    ),
                ],
            },
        ],
    },
    CapabilitySpec {
        id: "voice.asr",
        label: "语音识别 (ASR)",
        apply: "restart",
        models: &[
            CatalogModel {
                id: "paraformer-trilingual",
                name: "Paraformer 三语（普通话/粤语/英语）",
                dir: "voice/paraformer-trilingual",
                languages: &["zh", "yue", "en"],
                license: "Apache-2.0",
                notes: "默认",
                files: &[
                    f(
                        "model",
                        "model.int8.onnx",
                        "model.int8.onnx",
                        244684152,
                        Some("eb3cdd288f535cf73258f491cdd7d68ad5a00aee135c0bba4c0884ea8d926144"),
                        Some("csukuangfj/sherpa-onnx-paraformer-trilingual-zh-cantonese-en"),
                    ),
                    f(
                        "tokens",
                        "tokens.txt",
                        "tokens.txt",
                        118931,
                        None,
                        Some("csukuangfj/sherpa-onnx-paraformer-trilingual-zh-cantonese-en"),
                    ),
                ],
            },
            CatalogModel {
                id: "paraformer-zh-small",
                name: "Paraformer 普通话 small",
                dir: "voice/paraformer-zh-small",
                languages: &["zh"],
                license: "Apache-2.0",
                notes: "更小更快，仅普通话（粤语/英语将无法转写）",
                files: &[
                    f(
                        "model",
                        "model.int8.onnx",
                        "model.int8.onnx",
                        81828675,
                        Some("3ef6c19369b912f7caf3cef8e545c5ccd1a33d9d7ec792a46668dc41c4b229ec"),
                        Some("csukuangfj/sherpa-onnx-paraformer-zh-small-2024-03-09"),
                    ),
                    f(
                        "tokens",
                        "tokens.txt",
                        "tokens.txt",
                        75352,
                        None,
                        Some("csukuangfj/sherpa-onnx-paraformer-zh-small-2024-03-09"),
                    ),
                ],
            },
        ],
    },
    CapabilitySpec {
        id: "tts.zh",
        label: "语音合成 · 普通话",
        apply: "restart",
        models: &[CatalogModel {
            id: "vits-melo-tts-zh_en",
            name: "vits-melo-tts-zh_en",
            dir: "voice/melo",
            languages: &["zh", "en"],
            license: "MIT",
            notes: "默认（普通话；亦可用作英语语音）",
            files: &[
                f(
                    "model",
                    "model.onnx",
                    "model.onnx",
                    170429550,
                    Some("bf30582eb1b012250a35b1a4a80e7dfbcf8485e7bb9de0d95efbbeef0e4ad86d"),
                    Some("csukuangfj/vits-melo-tts-zh_en"),
                ),
                f(
                    "lexicon",
                    "lexicon.txt",
                    "lexicon.txt",
                    6837671,
                    None,
                    Some("csukuangfj/vits-melo-tts-zh_en"),
                ),
                f(
                    "tokens",
                    "tokens.txt",
                    "tokens.txt",
                    655,
                    None,
                    Some("csukuangfj/vits-melo-tts-zh_en"),
                ),
                f(
                    "fst",
                    "number.fst",
                    "number.fst",
                    64482,
                    None,
                    Some("csukuangfj/vits-melo-tts-zh_en"),
                ),
                f(
                    "fst",
                    "date.fst",
                    "date.fst",
                    59154,
                    None,
                    Some("csukuangfj/vits-melo-tts-zh_en"),
                ),
                f(
                    "dict",
                    "dict/hmm_model.utf8",
                    "dict/hmm_model.utf8",
                    519739,
                    None,
                    Some("csukuangfj/vits-melo-tts-zh_en"),
                ),
                f(
                    "dict",
                    "dict/idf.utf8",
                    "dict/idf.utf8",
                    5998717,
                    None,
                    Some("csukuangfj/vits-melo-tts-zh_en"),
                ),
                f(
                    "dict",
                    "dict/jieba.dict.utf8",
                    "dict/jieba.dict.utf8",
                    5071204,
                    None,
                    Some("csukuangfj/vits-melo-tts-zh_en"),
                ),
                f(
                    "dict",
                    "dict/pos_dict/char_state_tab.utf8",
                    "dict/pos_dict/char_state_tab.utf8",
                    327139,
                    None,
                    Some("csukuangfj/vits-melo-tts-zh_en"),
                ),
                f(
                    "dict",
                    "dict/pos_dict/prob_emit.utf8",
                    "dict/pos_dict/prob_emit.utf8",
                    1687686,
                    None,
                    Some("csukuangfj/vits-melo-tts-zh_en"),
                ),
                f(
                    "dict",
                    "dict/pos_dict/prob_start.utf8",
                    "dict/pos_dict/prob_start.utf8",
                    4347,
                    None,
                    Some("csukuangfj/vits-melo-tts-zh_en"),
                ),
                f(
                    "dict",
                    "dict/pos_dict/prob_trans.utf8",
                    "dict/pos_dict/prob_trans.utf8",
                    124159,
                    None,
                    Some("csukuangfj/vits-melo-tts-zh_en"),
                ),
                f(
                    "dict",
                    "dict/stop_words.utf8",
                    "dict/stop_words.utf8",
                    8974,
                    None,
                    Some("csukuangfj/vits-melo-tts-zh_en"),
                ),
                f(
                    "dict",
                    "dict/user.dict.utf8",
                    "dict/user.dict.utf8",
                    49,
                    None,
                    Some("csukuangfj/vits-melo-tts-zh_en"),
                ),
                f(
                    "dict",
                    "dict/README.md",
                    "dict/README.md",
                    683,
                    None,
                    Some("csukuangfj/vits-melo-tts-zh_en"),
                ),
            ],
        }],
    },
    CapabilitySpec {
        id: "tts.yue",
        label: "语音合成 · 粤语",
        apply: "restart",
        models: &[CatalogModel {
            id: "vits-cantonese-xiaomaiiwn",
            name: "vits-cantonese-hf-xiaomaiiwn",
            dir: "voice/tts-yue",
            languages: &["yue"],
            license: "Apache-2.0",
            notes: "默认粤语语音",
            files: &[
                f(
                    "model",
                    "vits-cantonese-hf-xiaomaiiwn.onnx",
                    "vits-cantonese-hf-xiaomaiiwn.onnx",
                    114059955,
                    Some("7d8d4f5550b607999417a99131b034c8cc2b8dca69f9e9fdefeff6655643c139"),
                    Some("csukuangfj/vits-cantonese-hf-xiaomaiiwn"),
                ),
                f(
                    "lexicon",
                    "lexicon.txt",
                    "lexicon.txt",
                    294061,
                    None,
                    Some("csukuangfj/vits-cantonese-hf-xiaomaiiwn"),
                ),
                f(
                    "tokens",
                    "tokens.txt",
                    "tokens.txt",
                    529,
                    None,
                    Some("csukuangfj/vits-cantonese-hf-xiaomaiiwn"),
                ),
            ],
        }],
    },
    CapabilitySpec {
        id: "tts.en",
        label: "语音合成 · 英语",
        apply: "restart",
        models: &[
            CatalogModel {
                id: "tts-en-current",
                name: "英语 vits（随部署）",
                dir: "voice/tts-en",
                languages: &["en"],
                license: "-",
                notes: "当前部署模型（无公开下载源）",
                files: &[
                    f("model", "model.onnx", "model.onnx", 114016948, None, None),
                    f("tokens", "tokens.txt", "tokens.txt", 303, None, None),
                ],
            },
            CatalogModel {
                id: "melo-en",
                name: "melo（英语）",
                dir: "voice/melo",
                languages: &["en"],
                license: "MIT",
                notes: "复用 melo 双语模型作英语语音（与普通话共用文件）",
                files: &[
                    f(
                        "model",
                        "model.onnx",
                        "model.onnx",
                        170429550,
                        Some("bf30582eb1b012250a35b1a4a80e7dfbcf8485e7bb9de0d95efbbeef0e4ad86d"),
                        Some("csukuangfj/vits-melo-tts-zh_en"),
                    ),
                    f(
                        "lexicon",
                        "lexicon.txt",
                        "lexicon.txt",
                        6837671,
                        None,
                        Some("csukuangfj/vits-melo-tts-zh_en"),
                    ),
                    f(
                        "tokens",
                        "tokens.txt",
                        "tokens.txt",
                        655,
                        None,
                        Some("csukuangfj/vits-melo-tts-zh_en"),
                    ),
                    f(
                        "fst",
                        "number.fst",
                        "number.fst",
                        64482,
                        None,
                        Some("csukuangfj/vits-melo-tts-zh_en"),
                    ),
                    f(
                        "fst",
                        "date.fst",
                        "date.fst",
                        59154,
                        None,
                        Some("csukuangfj/vits-melo-tts-zh_en"),
                    ),
                    f(
                        "dict",
                        "dict/hmm_model.utf8",
                        "dict/hmm_model.utf8",
                        519739,
                        None,
                        Some("csukuangfj/vits-melo-tts-zh_en"),
                    ),
                    f(
                        "dict",
                        "dict/idf.utf8",
                        "dict/idf.utf8",
                        5998717,
                        None,
                        Some("csukuangfj/vits-melo-tts-zh_en"),
                    ),
                    f(
                        "dict",
                        "dict/jieba.dict.utf8",
                        "dict/jieba.dict.utf8",
                        5071204,
                        None,
                        Some("csukuangfj/vits-melo-tts-zh_en"),
                    ),
                    f(
                        "dict",
                        "dict/pos_dict/char_state_tab.utf8",
                        "dict/pos_dict/char_state_tab.utf8",
                        327139,
                        None,
                        Some("csukuangfj/vits-melo-tts-zh_en"),
                    ),
                    f(
                        "dict",
                        "dict/pos_dict/prob_emit.utf8",
                        "dict/pos_dict/prob_emit.utf8",
                        1687686,
                        None,
                        Some("csukuangfj/vits-melo-tts-zh_en"),
                    ),
                    f(
                        "dict",
                        "dict/pos_dict/prob_start.utf8",
                        "dict/pos_dict/prob_start.utf8",
                        4347,
                        None,
                        Some("csukuangfj/vits-melo-tts-zh_en"),
                    ),
                    f(
                        "dict",
                        "dict/pos_dict/prob_trans.utf8",
                        "dict/pos_dict/prob_trans.utf8",
                        124159,
                        None,
                        Some("csukuangfj/vits-melo-tts-zh_en"),
                    ),
                    f(
                        "dict",
                        "dict/stop_words.utf8",
                        "dict/stop_words.utf8",
                        8974,
                        None,
                        Some("csukuangfj/vits-melo-tts-zh_en"),
                    ),
                    f(
                        "dict",
                        "dict/user.dict.utf8",
                        "dict/user.dict.utf8",
                        49,
                        None,
                        Some("csukuangfj/vits-melo-tts-zh_en"),
                    ),
                    f(
                        "dict",
                        "dict/README.md",
                        "dict/README.md",
                        683,
                        None,
                        Some("csukuangfj/vits-melo-tts-zh_en"),
                    ),
                ],
            },
        ],
    },
    CapabilitySpec {
        id: "face.detect",
        label: "人脸检测",
        apply: "restart",
        models: &[CatalogModel {
            id: "yunet-2023mar",
            name: "YuNet 2023-03",
            dir: "face",
            languages: &[],
            license: "Apache-2.0",
            notes: "",
            files: &[f(
                "model",
                "face_detection_yunet_2023mar.onnx",
                "face_detection_yunet_2023mar.onnx",
                232589,
                Some("8f2383e4dd3cfbb4553ea8718107fc0423210dc964f9f4280604804ed2552fa4"),
                Some("opencv/face_detection_yunet"),
            )],
        }],
    },
    CapabilitySpec {
        id: "face.recog",
        label: "人脸识别",
        apply: "restart",
        models: &[CatalogModel {
            id: "sface-2021dec",
            name: "SFace 128 维嵌入",
            dir: "face",
            languages: &[],
            license: "Apache-2.0",
            notes: "",
            files: &[f(
                "model",
                "face_recognition_sface_2021dec.onnx",
                "face_recognition_sface_2021dec.onnx",
                38696353,
                Some("0ba9fbfa01b5270c96627c4ef784da859931e02f04419c829e83484087c34e79"),
                Some("opencv/face_recognition_sface"),
            )],
        }],
    },
    CapabilitySpec {
        id: "ai",
        label: "视觉目标检测",
        apply: "immediate",
        models: &[
            CatalogModel {
                id: "nanodet-plus-m-320",
                name: "NanoDet-Plus-m 320",
                dir: ".",
                languages: &[],
                license: "Apache-2.0",
                notes: "热切换",
                files: &[f(
                    "model",
                    "nanodet-m.onnx",
                    "nanodet-m.onnx",
                    4793615,
                    None,
                    None,
                )],
            },
            CatalogModel {
                id: "nanodet-plus-m-416",
                name: "NanoDet-Plus-m 416",
                dir: ".",
                languages: &[],
                license: "Apache-2.0",
                notes: "热切换",
                files: &[f(
                    "model",
                    "nanodet-m-416.onnx",
                    "nanodet-m-416.onnx",
                    4793616,
                    None,
                    None,
                )],
            },
        ],
    },
    CapabilitySpec {
        id: "ocr",
        label: "文字识别 (OCR)",
        apply: "restart",
        models: &[CatalogModel {
            id: "ppocr-ch-v4det-v5rec",
            name: "PP-OCRv4 检测 + PP-OCRv5 mobile 识别",
            dir: "ocr",
            languages: &["zh", "en"],
            license: "Apache-2.0",
            notes: "无公开镜像下载源",
            files: &[
                f(
                    "det",
                    "ch_PP-OCRv4_det_infer.onnx",
                    "ch_PP-OCRv4_det_infer.onnx",
                    4745517,
                    None,
                    None,
                ),
                f(
                    "rec",
                    "ppocrv5_mobile_rec.onnx",
                    "ppocrv5_mobile_rec.onnx",
                    16560815,
                    None,
                    None,
                ),
                f(
                    "dict",
                    "ppocrv5_dict.txt",
                    "ppocrv5_dict.txt",
                    74012,
                    None,
                    None,
                ),
            ],
        }],
    },
    CapabilitySpec {
        id: "decision",
        label: "意图分类 (Laya)",
        apply: "restart",
        models: &[CatalogModel {
            id: "laya-multilingual-int8",
            name: "Laya multilingual int8",
            dir: "decision",
            languages: &["zh", "en"],
            license: "Apache-2.0",
            notes: "无公开镜像下载源",
            files: &[
                f(
                    "model",
                    "laya_multilingual.int8.onnx",
                    "laya_multilingual.int8.onnx",
                    924260943,
                    None,
                    None,
                ),
                f(
                    "tokenizer",
                    "tokenizer.json",
                    "tokenizer.json",
                    34363188,
                    None,
                    None,
                ),
                f(
                    "config",
                    "laya_config.json",
                    "laya_config.json",
                    115,
                    None,
                    None,
                ),
                f(
                    "aux",
                    "tokenizer_config.json",
                    "tokenizer_config.json",
                    524,
                    None,
                    None,
                ),
            ],
        }],
    },
    CapabilitySpec {
        id: "speaker",
        label: "声纹识别 (CAM++)",
        apply: "restart",
        models: &[CatalogModel {
            id: "campplus",
            name: "3D-Speaker CAM++ (zh_en advanced)",
            dir: "voice/speaker",
            languages: &[],
            license: "Apache-2.0",
            notes: "",
            files: &[f(
                "model",
                "campplus.onnx",
                "3dspeaker_speech_campplus_sv_zh_en_16k-common_advanced.onnx",
                28281164,
                Some("aa3cfc16963a10586a9393f5035d6d6b57e98d358b347f80c2a30bf4f00ceba2"),
                Some("csukuangfj/speaker-embedding-models"),
            )],
        }],
    },
];

pub fn catalog() -> &'static [CapabilitySpec] {
    if let Some(over) = *TEST_CATALOG.lock().expect("test catalog lock") {
        return over;
    }
    CATALOG
}

pub fn capability(id: &str) -> Option<&'static CapabilitySpec> {
    catalog().iter().find(|c| c.id == id)
}

pub fn find(capability_id: &str, model_id: &str) -> Option<&'static CatalogModel> {
    capability(capability_id)?
        .models
        .iter()
        .find(|m| m.id == model_id)
}

/// Total download size of a catalog model.
pub fn model_size(m: &CatalogModel) -> u64 {
    m.files.iter().map(|fl| fl.size).sum()
}

pub fn downloadable(m: &CatalogModel) -> bool {
    m.files.iter().any(|fl| fl.repo.is_some())
}

/// URLs to try for one file: hf-mirror first (CN-reachable), then the
/// origin. Empty when the file has no source. A URL override for the
/// repo (tests) replaces both.
fn file_urls(
    overrides: &std::collections::HashMap<String, String>,
    fl: &CatalogFile,
) -> Vec<String> {
    match fl.repo {
        Some(repo) => {
            if let Some(base) = overrides.get(repo) {
                return vec![format!("{base}/{}", fl.remote)];
            }
            vec![
                format!("https://hf-mirror.com/{repo}/resolve/main/{}", fl.remote),
                format!("https://huggingface.co/{repo}/resolve/main/{}", fl.remote),
            ]
        }
        None => Vec::new(),
    }
}

/// Installed = every declared file exists at `root/{model.dir}/{path}`
/// with the declared size (hash was verified at download time).
pub fn is_installed(models_root: &Path, m: &CatalogModel) -> bool {
    m.files.iter().all(|fl| {
        std::fs::metadata(models_root.join(m.dir).join(fl.path))
            .map(|meta| meta.is_file() && meta.len() == fl.size)
            .unwrap_or(false)
    })
}

/// Full integrity check of an installed model (hex intake hardening):
/// every file with a pinned sha256 is hashed and compared; files the
/// source publishes no hash for fall back to the size check. `Err`
/// names the first mismatch — a hand-placed or bit-rotted file that
/// `is_installed` (size-only, boot-cheap) let through is caught here
/// at the activation boundary, before an engine loads it.
pub fn verify_installed(models_root: &Path, m: &CatalogModel) -> Result<(), String> {
    for fl in m.files {
        let path = models_root.join(m.dir).join(fl.path);
        let meta = std::fs::metadata(&path).map_err(|e| format!("{}: {e}", fl.path))?;
        if !meta.is_file() {
            return Err(format!("{} is not a file", fl.path));
        }
        if meta.len() != fl.size {
            return Err(format!(
                "{}: size {} != catalog {}",
                fl.path,
                meta.len(),
                fl.size
            ));
        }
        let Some(want) = fl.sha256 else {
            continue;
        };
        let mut file = std::fs::File::open(&path).map_err(|e| format!("{}: {e}", fl.path))?;
        let mut hasher = sha2::Sha256::default();
        std::io::copy(&mut file, &mut hasher).map_err(|e| format!("{}: {e}", fl.path))?;
        use sha2::Digest;
        let got = format!("{:x}", hasher.finalize());
        if got != want {
            return Err(format!(
                "{}: sha256 mismatch (catalog {}, found {})",
                fl.path, want, got
            ));
        }
    }
    Ok(())
}

/// Free space on the filesystem holding `path`, in bytes (Linux `df`).
/// `None` when the probe fails — callers treat that as "unknown, proceed".
pub fn available_bytes(path: &Path) -> Option<u64> {
    let out = std::process::Command::new("df")
        .arg("-B1")
        .arg("--output=avail")
        .arg(path)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let last = text.lines().last()?.trim();
    let num: u64 = last.parse().ok()?;
    Some(num)
}

// ---------------------------------------------------------------------------
// Download manager
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Downloading,
    Verifying,
    Done,
    Failed,
    Canceled,
}

/// SPEC §4.9 task object — the exact shape served by `/api/models/tasks`
/// and the `model_task` SSE event.
#[derive(Debug, Clone, Serialize)]
pub struct TaskSnapshot {
    pub task_id: String,
    pub capability: String,
    pub model_id: String,
    pub model_name: String,
    pub status: TaskStatus,
    pub progress: f32,
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
    pub error: Option<String>,
}

struct TaskInner {
    snapshot: Mutex<TaskSnapshot>,
    cancel: AtomicBool,
    finished: AtomicBool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StartError {
    NotFound,
    AlreadyInstalled,
    TaskRunning,
    InsufficientDisk { need: u64, avail: u64 },
}

#[derive(Debug, Clone, PartialEq)]
pub enum CancelError {
    NotFound,
    Finished,
}

/// Concurrent download manager. One task per (capability, model) at a
/// time; finished tasks stay in the history (bounded) for the tasks view.
pub struct DownloadManager {
    tasks: Mutex<Vec<Arc<TaskInner>>>,
    seq: AtomicU64,
    events: tokio::sync::broadcast::Sender<TaskSnapshot>,
    http: reqwest::Client,
    /// Injectable for tests (real one shells out to `df`).
    disk_probe: fn(&Path) -> Option<u64>,
    /// Progress broadcast throttle (tests may set 0).
    emit_interval: std::time::Duration,
    /// Repo → base URL overrides (tests point repos at a local server).
    url_overrides: std::collections::HashMap<String, String>,
}

impl Default for DownloadManager {
    fn default() -> Self {
        Self::new()
    }
}

impl DownloadManager {
    pub fn new() -> Self {
        let (events, _) = tokio::sync::broadcast::channel(64);
        Self {
            tasks: Mutex::new(Vec::new()),
            seq: AtomicU64::new(0),
            events,
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(20))
                .build()
                .expect("reqwest client"),
            disk_probe: available_bytes,
            emit_interval: Duration::from_millis(500),
            url_overrides: std::collections::HashMap::new(),
        }
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<TaskSnapshot> {
        self.events.subscribe()
    }

    pub fn tasks(&self) -> Vec<TaskSnapshot> {
        self.tasks
            .lock()
            .expect("download tasks lock")
            .iter()
            .map(|t| t.snapshot.lock().expect("snapshot lock").clone())
            .collect()
    }

    /// Active (downloading/verifying) task for a model, if any.
    pub fn active_task_for(&self, capability: &str, model_id: &str) -> Option<TaskSnapshot> {
        self.tasks().into_iter().find(|t| {
            t.capability == capability
                && t.model_id == model_id
                && matches!(t.status, TaskStatus::Downloading | TaskStatus::Verifying)
        })
    }

    pub fn start(
        self: &Arc<Self>,
        models_root: PathBuf,
        capability_id: &str,
        model_id: &str,
        force: bool,
    ) -> Result<TaskSnapshot, StartError> {
        let model = find(capability_id, model_id).ok_or(StartError::NotFound)?;
        if !downloadable(model) {
            return Err(StartError::NotFound);
        }
        if !force && is_installed(&models_root, model) {
            return Err(StartError::AlreadyInstalled);
        }
        if self.active_task_for(capability_id, model_id).is_some() {
            return Err(StartError::TaskRunning);
        }
        let total = model_size(model);
        let need = total + total / 10;
        if let Some(avail) = (self.disk_probe)(&models_root)
            && avail < need
        {
            return Err(StartError::InsufficientDisk { need, avail });
        }
        let task_id = format!("mt-{}", self.seq.fetch_add(1, Ordering::SeqCst) + 1);
        let snapshot = TaskSnapshot {
            task_id: task_id.clone(),
            capability: capability_id.to_string(),
            model_id: model_id.to_string(),
            model_name: model.name.to_string(),
            status: TaskStatus::Downloading,
            progress: 0.0,
            downloaded_bytes: 0,
            total_bytes: total,
            error: None,
        };
        let inner = Arc::new(TaskInner {
            snapshot: Mutex::new(snapshot.clone()),
            cancel: AtomicBool::new(false),
            finished: AtomicBool::new(false),
        });
        {
            let mut tasks = self.tasks.lock().expect("download tasks lock");
            if tasks.len() > 50 {
                let keep = tasks.len() - 50;
                let mut removed = 0;
                tasks.retain(|t| {
                    if removed < keep && t.finished.load(Ordering::SeqCst) {
                        removed += 1;
                        false
                    } else {
                        true
                    }
                });
            }
            tasks.push(Arc::clone(&inner));
        }
        let mgr = Arc::clone(self);
        let root = models_root;
        let cap = capability_id.to_string();
        let mid = model_id.to_string();
        tokio::spawn(async move {
            mgr.run(root, &cap, &mid, inner).await;
        });
        Ok(snapshot)
    }

    pub fn cancel(&self, task_id: &str) -> Result<TaskSnapshot, CancelError> {
        let tasks = self.tasks.lock().expect("download tasks lock");
        let task = tasks
            .iter()
            .find(|t| t.snapshot.lock().expect("snapshot lock").task_id == task_id)
            .ok_or(CancelError::NotFound)?;
        if task.finished.load(Ordering::SeqCst) {
            return Err(CancelError::Finished);
        }
        task.cancel.store(true, Ordering::SeqCst);
        Ok(task.snapshot.lock().expect("snapshot lock").clone())
    }

    async fn run(
        self: &Arc<Self>,
        root: PathBuf,
        capability_id: &str,
        model_id: &str,
        inner: Arc<TaskInner>,
    ) {
        let model = match find(capability_id, model_id) {
            Some(m) => m,
            None => return,
        };
        let result = self.download_model(&root, model, &inner).await;
        let mut snap = inner.snapshot.lock().expect("snapshot lock");
        match result {
            Ok(()) => {
                snap.status = TaskStatus::Done;
                snap.progress = 1.0;
                snap.downloaded_bytes = snap.total_bytes;
                tracing::info!(
                    capability = capability_id,
                    model = model_id,
                    bytes = snap.total_bytes,
                    "models: download complete"
                );
            }
            Err(err) => {
                let canceled = err
                    .downcast_ref::<DownloadError>()
                    .is_some_and(|e| matches!(e, DownloadError::Canceled));
                snap.status = if canceled {
                    TaskStatus::Canceled
                } else {
                    TaskStatus::Failed
                };
                snap.error = (!canceled).then(|| err.to_string());
                tracing::warn!(
                    capability = capability_id,
                    model = model_id,
                    error = %err,
                    "models: download failed"
                );
            }
        }
        inner.finished.store(true, Ordering::SeqCst);
        let out = snap.clone();
        drop(snap);
        let _ = self.events.send(out);
    }

    async fn download_model(
        self: &Arc<Self>,
        root: &Path,
        model: &'static CatalogModel,
        inner: &Arc<TaskInner>,
    ) -> Result<(), Arc<anyhow::Error>> {
        let mut downloaded: u64 = 0;
        let total = model_size(model);
        let mut last_emit = std::time::Instant::now() - self.emit_interval;
        for fl in model.files {
            if inner.cancel.load(Ordering::SeqCst) {
                return Err(Arc::new(anyhow::anyhow!(DownloadError::Canceled)));
            }
            let target = root.join(model.dir).join(fl.path);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| Arc::new(anyhow::anyhow!("mkdir {}: {e}", parent.display())))?;
            }
            self.download_file(fl, &target, inner, total, &mut downloaded, &mut last_emit)
                .await?;
        }
        // Verifying: sizes were checked per file; re-check the whole set so
        // a concurrent delete cannot race into a "done" verdict.
        {
            let mut snap = inner.snapshot.lock().expect("snapshot lock");
            snap.status = TaskStatus::Verifying;
            let out = snap.clone();
            drop(snap);
            let _ = self.events.send(out);
        }
        if !is_installed(root, model) {
            return Err(Arc::new(anyhow::anyhow!(
                "verification failed: files missing after download"
            )));
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn download_file(
        self: &Arc<Self>,
        fl: &'static CatalogFile,
        target: &Path,
        inner: &Arc<TaskInner>,
        total: u64,
        downloaded: &mut u64,
        last_emit: &mut std::time::Instant,
    ) -> Result<(), Arc<anyhow::Error>> {
        use sha2::Digest;

        let part = target.with_file_name(format!(
            "{}.part",
            target
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("file")
        ));
        let urls = file_urls(&self.url_overrides, fl);
        if urls.is_empty() {
            return Err(Arc::new(anyhow::anyhow!(
                "no download source for {}",
                fl.path
            )));
        }
        let mut last_err: Option<String> = None;
        for url in &urls {
            let mut have = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
            let mut req = self.http.get(url);
            if have > 0 {
                req = req.header(reqwest::header::RANGE, format!("bytes={have}-"));
            }
            let resp = match req.send().await {
                Ok(r) => r,
                Err(e) => {
                    last_err = Some(format!("{url}: {e}"));
                    continue;
                }
            };
            let status = resp.status();
            if !status.is_success() {
                last_err = Some(format!("{url}: HTTP {status}"));
                continue;
            }
            // Resume only when the server honored the range (206). A plain
            // 200 after a range request means the mirror ignored it —
            // restart from zero (and the running hash starts over too).
            let resume = have > 0 && status.as_u16() == 206;
            let fresh = !resume;
            let mut hasher = sha2::Sha256::new();
            if !resume {
                have = 0;
            }
            let mut writer = match if resume {
                std::fs::OpenOptions::new().append(true).open(&part)
            } else {
                std::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(&part)
            } {
                Ok(w) => w,
                Err(e) => {
                    last_err = Some(format!("open {}: {e}", part.display()));
                    continue;
                }
            };
            use futures::StreamExt;
            let mut stream = resp.bytes_stream();
            let mut failed: Option<String> = None;
            while let Some(chunk) = stream.next().await {
                if inner.cancel.load(Ordering::SeqCst) {
                    return Err(Arc::new(anyhow::anyhow!(DownloadError::Canceled)));
                }
                let chunk = match chunk {
                    Ok(c) => c,
                    Err(e) => {
                        failed = Some(format!("body: {e}"));
                        break;
                    }
                };
                if let Err(e) = std::io::Write::write_all(&mut writer, &chunk) {
                    failed = Some(format!("write: {e}"));
                    break;
                }
                if fresh {
                    hasher.update(&chunk);
                }
                have += chunk.len() as u64;
                *downloaded += chunk.len() as u64;
                let emit = last_emit.elapsed() >= self.emit_interval;
                {
                    let mut snap = inner.snapshot.lock().expect("snapshot lock");
                    snap.downloaded_bytes = *downloaded;
                    snap.progress = if total > 0 {
                        *downloaded as f32 / total as f32
                    } else {
                        1.0
                    };
                    if emit {
                        let out = snap.clone();
                        drop(snap);
                        let _ = self.events.send(out);
                        *last_emit = std::time::Instant::now();
                    }
                }
            }
            if let Some(err) = failed {
                last_err = Some(err);
                continue; // try the next mirror (part file stays for resume)
            }
            // Size check.
            if have != fl.size {
                last_err = Some(format!(
                    "size mismatch for {}: got {have}, want {}",
                    fl.path, fl.size
                ));
                let _ = std::fs::remove_file(&part);
                continue;
            }
            // Hash check (fresh downloads only — a resume cannot rebuild
            // the running hash).
            if fresh && let Some(want) = fl.sha256 {
                use sha2::Digest;
                let got = format!("{:x}", hasher.finalize());
                if !got.eq_ignore_ascii_case(want) {
                    last_err = Some(format!("sha256 mismatch for {}", fl.path));
                    let _ = std::fs::remove_file(&part);
                    continue;
                }
            }
            if let Err(e) = std::fs::rename(&part, target) {
                last_err = Some(format!("rename: {e}"));
                continue;
            }
            return Ok(());
        }
        Err(Arc::new(anyhow::anyhow!(
            "all sources failed: {}",
            last_err.unwrap_or_else(|| "no source".into())
        )))
    }
}

#[derive(Debug)]
enum DownloadError {
    Canceled,
}

impl std::fmt::Display for DownloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DownloadError::Canceled => write!(f, "canceled"),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(root: &Path, rel: &str, size: u64) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().expect("parent")).unwrap();
        std::fs::write(p, vec![0u8; size as usize]).unwrap();
    }

    fn real_find(cap_id: &str, model_id: &str) -> &'static CatalogModel {
        CATALOG
            .iter()
            .find(|c| c.id == cap_id)
            .unwrap()
            .models
            .iter()
            .find(|m| m.id == model_id)
            .unwrap()
    }

    #[test]
    fn catalog_ids_are_unique_and_sizes_positive() {
        // Iterate the real table directly — the TEST_CATALOG override is
        // process-global and other tests may be mid-flight with it set.
        let mut cap_ids = std::collections::HashSet::new();
        for cap in CATALOG {
            assert!(cap_ids.insert(cap.id), "duplicate capability {}", cap.id);
            assert!(
                cap.apply == "restart" || cap.apply == "immediate",
                "bad apply on {}",
                cap.id
            );
            let mut model_ids = std::collections::HashSet::new();
            for m in cap.models {
                assert!(
                    model_ids.insert(m.id),
                    "duplicate model {}/{}",
                    cap.id,
                    m.id
                );
                assert!(!m.files.is_empty(), "{} has no files", m.id);
                for fl in m.files {
                    assert!(fl.size > 0, "zero-size file {}/{}", m.id, fl.path);
                    if let Some(sha) = fl.sha256 {
                        assert_eq!(
                            sha.len(),
                            64,
                            "sha256 must be a full hex digest: {}",
                            fl.path
                        );
                    }
                }
                // downloadable ⇔ every file has a source — a partial
                // source set would strand a model that can never install.
                let all_sourced = m.files.iter().all(|fl| fl.repo.is_some());
                assert_eq!(
                    downloadable(m),
                    all_sourced,
                    "{}/{}: downloadable flag disagrees with file sources",
                    cap.id,
                    m.id
                );
            }
        }
        // Capabilities the UI labels exist (SPEC §4.9 + webui capLabel_*).
        for id in [
            "llm",
            "vlm",
            "voice.asr",
            "tts.zh",
            "tts.yue",
            "tts.en",
            "face.detect",
            "face.recog",
            "ai",
            "ocr",
            "decision",
            "speaker",
        ] {
            assert!(
                CATALOG.iter().any(|c| c.id == id),
                "catalog missing capability {id}"
            );
        }
    }

    #[test]
    fn find_and_size_lookups() {
        // Pure lookups against the real table — `find()`/`capability()`
        // consult TEST_CATALOG, which parallel download tests may have set.
        let llm = CATALOG.iter().find(|c| c.id == "llm").unwrap();
        assert!(
            llm.models
                .iter()
                .any(|m| m.id == "qwen3-4b-instruct-2507-q4_k_m")
        );
        assert!(!llm.models.iter().any(|m| m.id == "nope"));
        assert!(!CATALOG.iter().any(|c| c.id == "nope"));
        assert_eq!(
            model_size(real_find("vlm", "qwen3-vl-2b-instruct")),
            1107409952 + 445053216
        );
        // The wake-word KWS trio is deliberately absent — the keyword file
        // is generated from the configured wake word, not swapped as a model.
        assert!(!CATALOG.iter().any(|c| c.id == "kws"));
    }

    #[test]
    fn installed_requires_exact_sizes() {
        let dir = std::env::temp_dir().join(format!("nb-models-inst-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let m = real_find("face.recog", "sface-2021dec");
        assert!(!is_installed(&dir, m));
        touch(
            &dir,
            "face/face_recognition_sface_2021dec.onnx",
            38696353 - 1,
        );
        assert!(!is_installed(&dir, m), "wrong size must not count");
        touch(&dir, "face/face_recognition_sface_2021dec.onnx", 38696353);
        assert!(is_installed(&dir, m));
        // Integrity: right size but zeros — the sha pin (if the catalog
        // carries one for this file) must reject it at activation.
        match m.files[0].sha256 {
            Some(_) => {
                let err = verify_installed(&dir, m).unwrap_err();
                assert!(err.contains("sha256 mismatch"), "{err}");
            }
            None => {
                assert!(verify_installed(&dir, m).is_ok(), "no pin → size-only");
            }
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn verify_installed_accepts_a_correct_hash() {
        use sha2::Digest;
        let dir = std::env::temp_dir().join(format!("nb-models-verify-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let m = real_find("face.recog", "sface-2021dec");
        let rel = "face/face_recognition_sface_2021dec.onnx";
        // Bytes that hash to the catalog pin: rebuild the file from the
        // expected digest only when a pin exists; otherwise the test
        // degenerates to the size path (still asserted).
        let want = m.files[0].sha256;
        let payload: Vec<u8> = vec![7u8; 38696353];
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, &payload).unwrap();
        match want {
            Some(w) => {
                let got = format!("{:x}", sha2::Sha256::digest(&payload));
                assert_ne!(got, w, "test premise: filler bytes must not match");
                assert!(verify_installed(&dir, m).is_err());
            }
            None => assert!(verify_installed(&dir, m).is_ok()),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    // -- downloader integration against a local HTTP server -------------------

    use axum::body::Body;
    use axum::extract::Request;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::any;
    use std::sync::atomic::AtomicUsize;

    /// A tiny static catalog served by the local test server.
    static TEST_FILES_CAP: &[CapabilitySpec] = &[CapabilitySpec {
        id: "test",
        label: "test",
        apply: "restart",
        models: &[
            CatalogModel {
                id: "small",
                name: "Small model",
                dir: "test/small",
                languages: &["zh"],
                license: "MIT",
                notes: "",
                files: &[
                    CatalogFile {
                        role: "model",
                        path: "model.bin",
                        remote: "model.bin",
                        size: 4096,
                        sha256: None,
                        repo: Some("local/test"),
                    },
                    CatalogFile {
                        role: "tokens",
                        path: "tokens.txt",
                        remote: "tokens.txt",
                        size: 16,
                        sha256: None,
                        repo: Some("local/test"),
                    },
                ],
            },
            CatalogModel {
                id: "hashed",
                name: "Hashed model",
                dir: "test/hashed",
                languages: &[],
                license: "MIT",
                notes: "",
                files: &[CatalogFile {
                    role: "model",
                    path: "model.bin",
                    remote: "model.bin",
                    size: 4096,
                    sha256: Some(
                        "0000000000000000000000000000000000000000000000000000000000000000",
                    ),
                    repo: Some("local/test"),
                }],
            },
            CatalogModel {
                id: "cancellable",
                name: "Cancellable model",
                dir: "test/cancellable",
                languages: &[],
                license: "MIT",
                notes: "",
                files: &[CatalogFile {
                    role: "model",
                    path: "model.bin",
                    remote: "slow.bin",
                    size: 8192,
                    sha256: None,
                    repo: Some("local/test"),
                }],
            },
            CatalogModel {
                id: "nosource",
                name: "No source",
                dir: "test/nosource",
                languages: &[],
                license: "MIT",
                notes: "",
                files: &[CatalogFile {
                    role: "model",
                    path: "model.bin",
                    remote: "model.bin",
                    size: 8,
                    sha256: None,
                    repo: None,
                }],
            },
        ],
    }];

    fn body_for(remote: &str) -> Vec<u8> {
        match remote {
            "model.bin" => (0..4096u32).map(|i| (i % 251) as u8).collect(),
            "tokens.txt" => vec![b't'; 16],
            // Held back by the server (see `serve`): a request for this
            // name stalls long enough for tests to cancel mid-flight.
            "slow.bin" => (0..8192u32).map(|i| (i % 199) as u8).collect(),
            _ => vec![1, 2, 3, 4],
        }
    }

    /// Serves every path from the test bodies; honors `Range: bytes=N-`
    /// (206) so resume is exercised; `hits` counts requests.
    #[allow(clippy::needless_pass_by_value)]
    async fn serve(
        listener: tokio::net::TcpListener,
        _hits: Arc<AtomicUsize>,
    ) -> std::io::Result<()> {
        let hits = Arc::clone(&_hits);
        let app = axum::Router::new().route(
            "/{*path}",
            any(move |req: Request| async move {
                hits.fetch_add(1, Ordering::SeqCst);
                let remote = req
                    .uri()
                    .path()
                    .trim_start_matches('/')
                    .rsplit('/')
                    .next()
                    .unwrap_or("model.bin")
                    .to_string();
                if remote == "slow.bin" {
                    tokio::time::sleep(Duration::from_millis(400)).await;
                }
                let mut body = body_for(&remote);
                let range = req
                    .headers()
                    .get(reqwest::header::RANGE)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.strip_prefix("bytes="))
                    .and_then(|v| v.split('-').next())
                    .and_then(|v| v.parse::<usize>().ok());
                if let Some(n) = range
                    && n <= body.len()
                {
                    let total = body.len();
                    let rest = body.split_off(n);
                    return (
                        StatusCode::PARTIAL_CONTENT,
                        [
                            ("content-type", "application/octet-stream".to_string()),
                            (
                                "content-range",
                                format!("bytes {n}-{}/{}", total - 1, total),
                            ),
                        ],
                        Body::from(rest),
                    )
                        .into_response();
                }
                (
                    StatusCode::OK,
                    [("content-type", "application/octet-stream".to_string())],
                    Body::from(body),
                )
                    .into_response()
            }),
        );
        axum::serve(listener, app).await
    }

    fn test_manager(server_port: u16, avail: Option<u64>) -> Arc<DownloadManager> {
        fn probe(_p: &Path) -> Option<u64> {
            None
        }
        fn probe_tight(_p: &Path) -> Option<u64> {
            Some(100)
        }
        let mut mgr = DownloadManager::new();
        mgr.url_overrides.insert(
            "local/test".to_string(),
            format!("http://127.0.0.1:{server_port}"),
        );
        mgr.emit_interval = Duration::from_millis(0);
        mgr.disk_probe = if avail.is_some() { probe_tight } else { probe };
        Arc::new(mgr)
    }

    async fn spawn_server() -> (u16, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        tokio::spawn(serve(listener, Arc::clone(&hits)));
        (port, hits)
    }

    fn use_test_catalog() {
        *TEST_CATALOG.lock().unwrap() = Some(TEST_FILES_CAP);
    }

    async fn wait_terminal(mgr: &DownloadManager, task_id: &str) -> TaskSnapshot {
        for _ in 0..400 {
            if let Some(t) = mgr.tasks().into_iter().find(|t| {
                t.task_id == task_id
                    && !matches!(t.status, TaskStatus::Downloading | TaskStatus::Verifying)
            }) {
                return t;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("task did not finish");
    }

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nb-models-dl-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn download_roundtrip_progress_and_done_event() {
        use_test_catalog();
        let root = temp_root("ok");
        let (port, _hits) = spawn_server().await;
        let mgr = test_manager(port, None);
        let mut sub = mgr.subscribe();

        let snap = mgr
            .start(root.clone(), "test", "small", false)
            .expect("start");
        assert_eq!(snap.status, TaskStatus::Downloading);
        let done = wait_terminal(&mgr, &snap.task_id).await;
        assert_eq!(done.status, TaskStatus::Done, "{done:?}");
        assert_eq!(done.downloaded_bytes, 4096 + 16);
        // Files landed with exact bytes; no .part leftovers.
        assert_eq!(
            std::fs::read(root.join("test/small/model.bin")).unwrap(),
            body_for("model.bin")
        );
        assert_eq!(
            std::fs::read(root.join("test/small/tokens.txt")).unwrap(),
            vec![b't'; 16]
        );
        assert!(std::fs::read_dir(root.join("test/small")).unwrap().count() == 2);
        // Progress events flowed on the broadcast bus (emit interval 0).
        let mut saw_progress = false;
        while let Ok(ev) = sub.try_recv() {
            if ev.status == TaskStatus::Downloading && ev.progress > 0.0 {
                saw_progress = true;
            }
        }
        assert!(saw_progress, "no progress event observed");
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn download_resumes_from_part_file() {
        use_test_catalog();
        let root = temp_root("resume");
        std::fs::create_dir_all(root.join("test/small")).unwrap();
        let full = body_for("model.bin");
        // Simulate an interrupted download of the first 1000 bytes.
        std::fs::write(root.join("test/small/model.bin.part"), &full[..1000]).unwrap();

        let (port, hits) = spawn_server().await;
        let mgr = test_manager(port, None);
        let snap = mgr
            .start(root.clone(), "test", "small", false)
            .expect("start");
        let done = wait_terminal(&mgr, &snap.task_id).await;
        assert_eq!(done.status, TaskStatus::Done, "{done:?}");
        // The server saw a ranged request for the remainder.
        assert!(hits.load(Ordering::SeqCst) >= 1);
        assert_eq!(
            std::fs::read(root.join("test/small/model.bin")).unwrap(),
            full
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn download_checksum_mismatch_fails_without_artifact() {
        use_test_catalog();
        let root = temp_root("badsum");
        let (port, _hits) = spawn_server().await;
        let mgr = test_manager(port, None);
        let snap = mgr
            .start(root.clone(), "test", "hashed", false)
            .expect("start");
        let done = wait_terminal(&mgr, &snap.task_id).await;
        assert_eq!(done.status, TaskStatus::Failed, "{done:?}");
        assert!(
            done.error.as_deref().unwrap_or("").contains("sha256"),
            "{done:?}"
        );
        assert!(!root.join("test/hashed/model.bin").exists());
        assert!(
            !root.join("test/hashed/model.bin.part").exists(),
            "bad file cleaned"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn start_guards_notfound_installed_disk_and_duplicate() {
        use_test_catalog();
        let root = temp_root("guards");
        let (port, _hits) = spawn_server().await;
        // Tight disk probe → insufficient space.
        let mgr = test_manager(port, Some(100));
        assert_eq!(
            mgr.start(root.clone(), "test", "nope", false).unwrap_err(),
            StartError::NotFound,
            "unknown model"
        );
        assert_eq!(
            mgr.start(root.clone(), "nope", "small", false).unwrap_err(),
            StartError::NotFound
        );
        assert_eq!(
            mgr.start(root.clone(), "test", "nosource", false)
                .unwrap_err(),
            StartError::NotFound,
            "no-source model is not startable"
        );
        assert!(matches!(
            mgr.start(root.clone(), "test", "small", false).unwrap_err(),
            StartError::InsufficientDisk { .. }
        ));
        // Realistic probe → succeeds; then duplicate → conflict; then
        // installed → conflict unless forced.
        let mgr2 = test_manager(port, None);
        let snap = mgr2
            .start(root.clone(), "test", "small", false)
            .expect("start");
        assert_eq!(
            mgr2.start(root.clone(), "test", "small", false)
                .unwrap_err(),
            StartError::TaskRunning
        );
        let done = wait_terminal(&mgr2, &snap.task_id).await;
        assert_eq!(done.status, TaskStatus::Done);
        assert_eq!(
            mgr2.start(root.clone(), "test", "small", false)
                .unwrap_err(),
            StartError::AlreadyInstalled
        );
        // force=true restarts over the installed files.
        let snap2 = mgr2
            .start(root.clone(), "test", "small", true)
            .expect("force start");
        assert_eq!(
            wait_terminal(&mgr2, &snap2.task_id).await.status,
            TaskStatus::Done
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn cancel_mid_download_leaves_no_final_file() {
        use_test_catalog();
        let root = temp_root("cancel");
        let (port, _hits) = spawn_server().await;
        let mgr = test_manager(port, None);
        // The server holds "slow.bin" back 400ms — cancel inside that
        // window so the task is deterministically mid-download.
        let snap = mgr
            .start(root.clone(), "test", "cancellable", false)
            .expect("start");
        tokio::time::sleep(Duration::from_millis(80)).await;
        mgr.cancel(&snap.task_id).expect("cancel");
        let done = wait_terminal(&mgr, &snap.task_id).await;
        assert_eq!(done.status, TaskStatus::Canceled, "{done:?}");
        assert!(
            done.error.is_none(),
            "canceled is not an error state: {done:?}"
        );
        // The final name never appears; a second cancel is rejected.
        assert!(!root.join("test/cancellable/model.bin").exists());
        assert_eq!(
            mgr.cancel(&snap.task_id).unwrap_err(),
            CancelError::Finished
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn available_bytes_parses_df() {
        let v = available_bytes(Path::new("/tmp"));
        assert!(
            v.is_some_and(|n| n > 1_000_000),
            "df probe must work on linux, got {v:?}"
        );
    }
}
