//! 文档↔代码断言：防止对外叙事与 Transport 注册表漂移（中英双语文档各自守护）。
//!
//! 背景：2026-10-01 大仓审计（infra4agent/docs/DOC_CODE_AUDIT-2026-10-01.md）
//! 声称本文件已落地，实际缺失；同审计期发现 README 曾长期写「iLink is the
//! only real adapter today」，而 `src/bridge/transport/registry.rs` 早已注册
//! 五个内置 Transport（ilink/telegram/wecom/feishu/discord），CI 从未拦截。
//!
//! 全部为纯 std 断言（include_str / fs 读文件 + contains），零依赖、无需
//! 网络与外部 CLI。
//!
//! 边界（jeffkit/im-agentproc#18）：`docs/guide/configuration.md` 与
//! `docs/zh/guide/configuration.md` 的 `via:` "(ilink only)" 表述**是正确的**
//! —— `registry.rs` 中只有 `ilink_factory` 消费 `ctx.via` —— 本文件没有
//! 任何断言会禁止它；往 `banned` 里加关键词时须避开这两行。

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

const REGISTRY: &str = include_str!("../src/bridge/transport/registry.rs");
const README: &str = include_str!("../README.md");
const AGENTS: &str = include_str!("../AGENTS.md");
const CARGO: &str = include_str!("../Cargo.toml");
const LIB: &str = include_str!("../src/lib.rs");
const INDEX_EN: &str = include_str!("../docs/index.md");
const INDEX_ZH: &str = include_str!("../docs/zh/index.md");
const CONFIG_EN: &str = include_str!("../docs/guide/configuration.md");
const CONFIG_ZH: &str = include_str!("../docs/zh/guide/configuration.md");
const QUICKSTART_EN: &str = include_str!("../docs/guide/quickstart.md");
const QUICKSTART_ZH: &str = include_str!("../docs/zh/guide/quickstart.md");
const TRANSPORT_EN: &str = include_str!("../docs/transport.md");
const TRANSPORT_ZH: &str = include_str!("../docs/zh/transport.md");

/// 解析 git index（v2），返回仓库当前跟踪的相对路径。cargo 集成测试的
/// cwd 就是 crate 根，因此无需定位仓库顶层；worktree（`.git` 是指向
/// `<main>/.git/worktrees/<name>` 的指针文件）也覆盖到。布局：12 字节头
/// （`DIRC` + 版本 + 条目数）后紧跟条目，每条目 62 字节固定前缀 + NUL
/// 结尾的路径，整个条目（62 + 路径长 + 1）按 8 字节对齐；条目数取自
/// 头部计数，因此扩展区（TREE/REUC 等）与尾部 20 字节校验和不会被误读。
fn tracked_files() -> Vec<String> {
    let index_path = if Path::new(".git").is_dir() {
        ".git/index".to_string()
    } else {
        let pointer = fs::read_to_string(".git").expect("read .git pointer");
        let gitdir = pointer
            .lines()
            .find_map(|l| l.strip_prefix("gitdir: "))
            .expect(".git pointer 缺少 gitdir:")
            .trim()
            .to_string();
        format!("{gitdir}/index")
    };
    let data = fs::read(&index_path).expect("read git index");
    assert!(
        data.len() >= 12 && data.starts_with(b"DIRC"),
        "git index 头不合法（期望 `DIRC` + 版本 + 条目数）：解析器与格式漂移？"
    );
    let version = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    assert_eq!(
        version, 2,
        "git index 版本 {version}：本解析器只实现了 v2 定长路径（v4 路径压缩会产出垃圾路径），\
         请勿盲目放行——改为走 `git ls-files` 或实现对应版本"
    );
    let entry_count = u32::from_be_bytes([data[8], data[9], data[10], data[11]]) as usize;
    let mut files = Vec::new();
    let mut i = 12;
    for _ in 0..entry_count {
        if i + 62 > data.len() {
            break;
        }
        let path_start = i + 62;
        let rest = &data[path_start..];
        let nul = match rest.iter().position(|&b| b == 0) {
            Some(n) => n,
            None => break,
        };
        files.push(String::from_utf8_lossy(&rest[..nul]).into_owned());
        // 终止 NUL 计入条目长度，整体补齐到 8 的倍数（pad 0..=7）。
        let total = 62 + nul + 1;
        let pad = (8 - (total % 8)) % 8;
        i += total + pad;
    }
    assert_eq!(
        files.len(),
        entry_count,
        "git index 条目解析不完整（{}/{}）：解析器漂移会让禁令测试空转（假绿）",
        files.len(),
        entry_count
    );
    files.retain(|f| {
        matches!(
            Path::new(f).extension().and_then(|e| e.to_str()),
            Some("md" | "toml" | "rs" | "yaml" | "yml")
        )
    });
    assert!(
        files.iter().any(|f| f == "src/lib.rs"),
        "git index 解析结果缺少 src/lib.rs：禁令测试可能空转（假绿）"
    );
    files
}

/// 五个真实适配器 kind（registry.rs `with_builtins` / `with_mcp_builtins` 顺序）。
const FIVE: [(&str, &str); 5] = [
    ("ilink", "iLink/WeChat"),
    ("telegram", "Telegram"),
    ("wecom", "WeCom"),
    ("feishu", "Feishu"),
    ("discord", "Discord"),
];

fn assert_contains_all(label: &str, text: &str, needles: &[&str]) {
    for n in needles {
        assert!(
            text.contains(n),
            "{label}: 缺少通道表述 `{n}`（五通道叙事漂移？）"
        );
    }
}

/// 中英两套「五通道清单」字面量：英文页 / 中文页各自的 hero feature 措辞。
const LIST_EN: &str = "iLink/WeChat, Telegram, WeCom, Feishu and Discord";
const LIST_ZH: &str = "iLink/微信、Telegram、WeCom、飞书、Discord";

/// #1 注册表 ↔ README / Cargo.toml description 一致：`with_builtins()` 注册的
/// kind 与 crates.io / GitHub 上的对外门面声明一致。
#[test]
fn registry_kinds_match_readme_and_cargo_description() {
    for (kind, _) in FIVE {
        assert!(
            REGISTRY.contains(&format!("reg.register(\"{kind}\"")),
            "registry.rs 不再注册 `{kind}`：同步本文件与全部对外文档"
        );
    }
    assert!(
        REGISTRY.contains("reg.register(\"null\""),
        "registry.rs 丢失 null 占位注册"
    );

    // README hero 与 Highlights 都点名五个通道。
    assert_contains_all(
        "README",
        README,
        &["iLink/WeChat, Telegram, WeCom, Feishu and Discord"],
    );
    assert!(
        README.contains("five adapters ship in-tree"),
        "README Highlights 缺少「five adapters ship in-tree」数量声明"
    );

    // Cargo.toml description 是 crates.io 上的对外门面，保持单行。
    let desc_line = CARGO
        .lines()
        .find(|l| l.trim_start().starts_with("description ="))
        .expect("Cargo.toml 缺少 description");
    for (_, label) in FIVE {
        assert!(
            desc_line.contains(label),
            "Cargo.toml description 缺少通道 {label}：{desc_line}"
        );
    }
}

/// #1(补) 镜像叙事面：crate doc、中英 hero feature、双语 quickstart 故障表、
/// 双语 configuration transport 字段行都要点名五通道，防止「只修 README 一处」
/// 的单点回归。中文侧措辞是「iLink/微信、Telegram、WeCom、飞书、Discord」。
#[test]
fn mirrored_surfaces_all_name_five_transports() {
    assert_contains_all(
        "src/lib.rs crate doc",
        LIB,
        &["iLink/WeChat, Telegram, WeCom, Feishu and Discord"],
    );
    for (label, text) in [("docs/index.md", INDEX_EN), ("docs/zh/index.md", INDEX_ZH)] {
        assert!(
            text.contains(LIST_EN) || text.contains(LIST_ZH),
            "{label}: hero feature 缺少五通道清单（EN=`{LIST_EN}` / ZH=`{LIST_ZH}`）"
        );
    }
    for (label, text) in [
        ("docs/guide/quickstart.md", QUICKSTART_EN),
        ("docs/zh/guide/quickstart.md", QUICKSTART_ZH),
    ] {
        assert!(
            text.contains("`ilink`, `telegram`, `wecom`, `feishu` and `discord`")
                || text.contains("`ilink`、`telegram`、`wecom`、`feishu`、`discord`"),
            "{label}: 故障表缺少内置五 kind 清单"
        );
    }
    assert_contains_all(
        "docs/guide/configuration.md transport 字段",
        CONFIG_EN,
        &["`ilink`, `telegram`, `wecom`, `feishu`, `discord`"],
    );
    assert_contains_all(
        "docs/zh/guide/configuration.md transport 字段",
        CONFIG_ZH,
        &["`ilink`、`telegram`、`wecom`、`feishu`、`discord`"],
    );
}

/// #2 旧叙事关键词禁令（git grep 级别：解析 `.git/index` 拿全部跟踪文本文件）。
/// 关键词都含通道名，不会误伤 configuration.md 的 `via:` "(ilink only)"。
#[test]
fn no_stale_single_channel_copy_anywhere() {
    let banned = [
        "only real adapter",
        "iLink/WeChat today",
        "Only ilink is implemented today",
        "唯一适配器",
        "唯一的适配器",
        "唯一真实适配器",
        "只有 ilink 一个",
        "当前仅 ilink",
        "目前仅实现了 ilink",
        "只实现了 ilink",
        "只支持 ilink",
    ];
    let mut offenders = Vec::new();
    for file in tracked_files() {
        // 本文件自身一旦入库（本测试存在的意义所在）就会进入 index，其源码里
        // 的 `banned` 字面量与本文件 doc 注释必然逐词命中全部关键词——这不是
        // 漂移，是扫描器看见了自己，必须跳过。
        if file == "tests/doc_claims.rs" {
            continue;
        }
        let Ok(text) = fs::read_to_string(&file) else {
            continue;
        };
        for kw in banned {
            if text.contains(kw) {
                offenders.push(format!("{file}: {kw}"));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "残留单通道旧叙事（registry.rs 已内置五通道，对外文案须同步）：{offenders:?}"
    );
}

/// #2(补) zh/transport.md 的五通道覆盖：四个非 iLink 内置通道必须成对出现
/// 中英接入指南；英文 transport 页的五个 ✅ 行齐全。zh 镜像仍存在两处单通道
/// 旧叙事（第 3 行开场「当前 iLink；未来飞书 / …」、适配器表只有 ilink 一行），
/// 由 zh_transport_adapters_table_todo_gate 跟踪，修文案归 #17。
#[test]
fn zh_transport_table_mirrors_en_implemented_adapters() {
    for (kind, _) in FIVE {
        let en_row = format!("| `{kind}` | ✅ implemented");
        assert!(
            TRANSPORT_EN.contains(&en_row),
            "docs/transport.md 已实现适配器表缺少 `{en_row}`"
        );
        // 每个非 iLink 内置通道都有一对中英接入指南页（ilink 走 quickstart）。
        if kind != "ilink" {
            for prefix in ["docs", "docs/zh"] {
                let page = format!("{prefix}/guide/{kind}.md");
                assert!(
                    Path::new(&page).is_file(),
                    "缺少接入指南页 {page}（对应 transport `{kind}`）"
                );
            }
        }
    }
}

/// zh/transport.md 与英文 transport.md 的「已实现适配器」表逐行对照（#3 的
/// transport 页部分）。EN 侧五个 ✅ 行已核对（见
/// `zh_transport_table_mirrors_en_implemented_adapters`）；zh 镜像表当前仍只有
/// ilink 一行、开场仍是「当前 iLink；未来…」——修文案归 #17，届时去掉
/// `#[ignore]` 让本锁生效。
#[test]
#[ignore = "#17 补齐 docs/zh/transport.md 适配器表后去掉此属性，锁生效"]
fn zh_transport_adapters_table_matches_en() {
    for (kind, _) in FIVE {
        assert!(
            TRANSPORT_ZH.contains(&format!("| `{kind}` | ✅ 已实现")),
            "docs/zh/transport.md 已实现适配器表缺少 `{kind}` 行（EN 侧已有）"
        );
    }
    assert!(
        !TRANSPORT_ZH.contains("当前 iLink；未来"),
        "docs/zh/transport.md 开场仍是「当前 iLink；未来飞书 / …」单通道旧叙事"
    );
}

/// #3 中英双语页面声明的通道集合一致：对每页抽取「出现的 kind 集合」互相比
/// 较（两侧语言句式不同，但五通道能力声明必须同集合、且满五个）。quickstart
/// 与 configuration 双侧都把 kind 词表写进了代码体；transport 英文页有五个
/// ✅ 行而 zh 镜像曾只有 ilink 一行，故 transport 页单独对照适配器表
/// （见 zh_transport_table_mirrors_en_implemented_adapters）。
#[test]
fn bilingual_pages_declare_same_channel_sets() {
    let kinds = ["ilink", "telegram", "wecom", "feishu", "discord"];
    for (page, en, zh) in [
        ("guide/quickstart", QUICKSTART_EN, QUICKSTART_ZH),
        ("guide/configuration", CONFIG_EN, CONFIG_ZH),
    ] {
        let set = |text: &str| -> BTreeSet<&str> {
            kinds.iter().copied().filter(|k| text.contains(k)).collect()
        };
        let (en_set, zh_set) = (set(en), set(zh));
        assert_eq!(
            en_set, zh_set,
            "{page}: 中英页面声明的通道集合不一致（一侧漏改？）"
        );
        assert_eq!(en_set.len(), 5, "{page}: 通道清单不满五个（EN={en_set:?}）");
    }
}

/// AGENTS.md（仓内导航入口）不退回旧叙事：五个适配器逐一列名、profile 内置
/// 清单与 `src/bridge/builtin/` 一致；agentproc 依赖保持 crates.io 口径
/// （大仓 DOC_CODE_AUDIT 表的另一项声称，一并守住）。
///
/// 不再断言「四种入口」：AGENTS.md 的三种模式表述与 docs/cli.md 同步漂移，
/// 文案修正归 jeffkit/im-agentproc#17，本文件只守通道清单。
#[test]
fn agents_md_and_cargo_dep_stay_aligned() {
    for adapter in [
        "IlinkTransport",
        "TelegramTransport",
        "WecomTransport",
        "FeishuTransport",
        "DiscordTransport",
    ] {
        assert!(
            AGENTS.contains(adapter),
            "AGENTS.md 架构地图缺少适配器 {adapter}"
        );
    }
    let line = CARGO
        .lines()
        .find(|l| l.trim_start().starts_with("agentproc"))
        .expect("Cargo.toml 缺少 agentproc 依赖");
    assert!(
        line.contains("0.11"),
        "agentproc crate 版本口径变化，请同步 README/大仓 ARCHITECTURE §4.1：{line}"
    );
    assert!(
        !line.contains("git =") && !line.contains("path ="),
        "agentproc 必须是 crates.io 依赖（大仓口径），不允许 git/path pin：{line}"
    );
}
