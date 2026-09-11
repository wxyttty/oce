# Agent Note: Model2Vec 静态查表嵌入为默认向量化，OpenAI 兼容 API 为可选

Status: implemented

## Problem

Rust 版此前必须配置 `EMBED_API_KEY`（或 model_credentials 里的 embed 凭据）才能索引——
零配置开箱体验依赖外部 API。同时用户需要在 oce（自带向量化）与 semble（纯词法检索）
之间做对比，决定最终是把 embedding 接入 semble 还是直接用 oce：对比需要一个**无网络、
零成本、可复现**的向量化基线。

## Decision

`rust/crates/oce-infra/src/static_embed.rs` 通过官方 Rust 实现
`model2vec-rs`（MinishLab）接入静态查表模型，作为**默认**向量化提供方：

- `EMBED_PROVIDER = auto | static | openai`（默认 `auto`：配置了 `EMBED_API_KEY`
  或 model_credentials 有效 embed 凭据 → openai；否则 → static）。
- `EMBED_STATIC_MODEL`：HF repo id 或本地目录（默认 `minishlab/potion-multilingual-128M`，
  256 维）；本地目录存在则直接加载，否则经 hf-hub 下载缓存。
- 维度自动解析：static 提供方的维度以模型文件为准，容器装配时覆盖
  `EMBED_DIMENSIONS`/`MILVUS_DENSE_DIM` 再打开 TriviumDB；若既有 `.tdb` 维度与
  模型不符，报错并提示显式设置（TriviumDB 维度创建后不可变）。
- OpenAI 兼容客户端保留原语义（batch/分段/池化/凭据解析/热重载不变），只是从
  「必需」降级为「可选提供方」。

## 真实工作集验证后的修正（同日）

jcfx（Java/Vue 中文业务仓库）实测暴露 potion-base-8M 致命缺陷：探针显示中文业务查询
margin **-0.10**（无关块得分反超正例），字段名查询 +0.31——与用户观察「中文业务词失效、
英文字段名命中」完全一致。三模型对比后默认切到 `potion-multilingual-128M`
（中文 +0.23 / 字段名 +0.34，双强）。同时新增 `.tdb.model` 指纹 sidecar：
同维度不同模型的向量混入同一索引会静默污染（维度校验拦不住 256↔256），换模型
fail-closed 强制重建。`dist-*` 构建产物目录加入服务端准入过滤（实测混入稀释结果）。

## Alternatives considered

- **自研加载器**（tokenizers + safetensors 手拼）：model2vec-rs 已处理 f32/f16/I8
  量化、weights/mapping 附加张量、normalize 配置等细节，自研只增加漂移风险。
- **本地推理模型**（fastembed/candle 跑 BGE 等）：质量更高但引入数百 MB 模型与
  数量级更慢的编码；对比基线与个人模式默认场景要的是「毫秒级、零依赖」。
- **把静态嵌入做成唯一提供方、删除 OpenAI 路径**：能力回退（credentials/重载语义
  是既有 admin 面），且对比实验需要两种提供方可切换。

## Consequences

- 收益：零配置索引/检索（下载一次模型后完全离线）；potion-base-8M 为 8M 参数查表，
  编码吞吐比 API 快数量级且免费；向量化确定性可复现，评测对比不含网络噪声。
- 代价：静态嵌入质量低于神经 embedding（检索质量下降是已知取舍，Model2Vec 论文
  场景下保持约 85-95% 相对质量）；两种提供方并存要求维度解析顺序必须清晰
  （static 维度覆盖配置，openai 维度沿用 EMBED_DIMENSIONS 并校验）。
- 对比用途：`oce-bench static-embed` 输出编码吞吐（实测 potion-base-8M
  4057 texts/s @ dim=256）；嵌入实现集中在 `Embedder` trait 后面，若最终决定接入
  semble，可直接复用该模块。
- 运维面：`oce doctor`（只读）验证模型可加载与维度一致；迁移场景先跑 doctor 再切流。
- BM25 混合检索（TriviumDB `enable_text_hybrid_search` + `index_text` + `build_text_index`，
  CJK 2-gram 分词）在 oce-benchmark 实测：flask 47.8%→**60.3%**（Top-1 39%→52%），
  cc-switch 31.1%→32.5%——中文/词法兜底是静态嵌入的最大单项增益，默认开启
  （`TRIVIUM_TEXT_HYBRID=true`）。`potion-code-16M-v2` 探针 margin 最高但全基准整体
  不敌多语言模型，维持多语言默认。
- 教训：改引擎代码后必须重建 release 再跑基准——第一轮「分数完全未变」即旧二进制所致
  （时间线核对发现 benchmark 跑在 hybrid 代码合入前的 binary 上）。
- 附带修复：LLM 重排失败原实现会清空召回，已改为静默退回原始顺序（对齐 Python
  语义）——在静态嵌入 + 无 LLM key 的默认配置下由 HTTP 全链路冒烟暴露。
