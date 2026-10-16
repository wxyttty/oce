# Rust 版专属环境变量（Python 版忽略）
#
# 这些键不在 Python Settings 元数据里，根 .env.example 由
# scripts/generate_env_template.py 自动生成、不含它们，故独立维护于此。
# 完整说明见 rust/README.md。

# ---- TriviumDB（向量库） ----
# TRIVIUM_TEXT_HYBRID=true                # BM25+dense RRF 混合（标识符门控后 +4.4 分）
# TRIVIUM_TEXT_BOOST=0.3                  # BM25 融合权重（0.3 实测最优；1.5 会 -20 分）
# TRIVIUM_EXPAND_DEPTH=0                  # 图扩散深度（1 会 -33 分，保持 0）

# ---- 嵌入 provider ----
# EMBED_PROVIDER=auto                     # auto|openai|static|local（local=candle CPU）
# EMBED_STATIC_MODEL=minishlab/potion-base-8M    # static provider 的模型
# EMBED_LOCAL_MODEL=Qwen/Qwen3-Embedding-0.6B    # local provider 的 candle 模型
# EMBED_LOCAL_DTYPE=f32                   # local 精度 f32|f16|bf16

# ---- 选择器 rank 分离 [实验，A/B 通过] ----
# rerank 分数饱和（多个 0.9+ 候选几乎同分）时，用组内 rank 的指数衰减
# exp(-rank/8) 打破平局——排名靠前者优先入席（对方 EvidenceEngine value() 同款）。
# 2026-10-09 行级覆盖 A/B（click/httpx/zod 60 题，同日单变量）：
# cov@4000 94.07% → 95.14%（+1.07）；0=关，0.5=推荐值。
# RETRIEVAL_SELECTOR_RANK_WEIGHT=0.5
