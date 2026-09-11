# 完全体检索质量：增益分解与 prompt 保真教训

日期：2026-09-05　状态：implemented　类别：architecture/quality

## 背景

Rust 版 oce 完成 BM25 混合检索后，LLM 组件（rerank/rewrite/intent）首次实测
全量基准。基准：oce-benchmark（flask、cc-switch 各 100 题，Top-1 + nDCG@10）。

## 决策与结果

1. 三处 LLM 组件的 prompt 必须从 Python `prompts.py` **逐字移植（AST 提取）**，
   禁止意译缩写。教训：首轮基准 flask 只有 3% Top-1，定位为 rerank prompt 丢失
   few-shot 示例——小参数模型对 prompt 结构高度敏感（Python 注释早已言明），
   逐字移植后消融测得 rerank 单开即拿走几乎全部 LLM 增益。
2. 模型回落链：`RETRIEVAL_QUERY_REWRITE_MODEL` 原本硬编码回落 Qwen2.5-7B，
   用户换 `LLM_MODEL` 时 rewrite 会悄悄掉回小模型。已改为
   显式设置 > LLM_MODEL > 内置默认，三处调用点跟随同一模型。
3. intent 分类单开实测负收益（top1 与 nDCG 均低于基线），建议默认关闭。

## 基准数据（完全体 vs 基线 vs ACE）

| 配置 | flask | cc-switch |
|---|---|---|
| 静态嵌入+混合·无LLM | 120.58 (60.3%) | 65.08 (32.5%) |
| API 嵌入 Qwen3-4B+混合·无LLM | 140.73 (70.4%) | 95.31 (47.7%) |
| API 嵌入+混合+LLM 全开 | **190.29 (95.1%)** | **157.26 (78.6%)** |
| （参照）ACE Finnian | 150.61 (75.3%) | 134.73 (67.4%) |

增益分解：嵌入升级 +20/+30 分，LLM 层 +50/+62 分；两杠杆相乘（更好嵌入给
rerank 更干净的候选池）。

## 备选方案取舍

- 重写 rmcp/自研 prompt：拒绝——保真优先，AST 提取杜绝转录错误。
- intent 保留默认开启：拒绝——实测负收益，等有更强的分类模型再评估。

## 后续（2026-09-05 追记）

- 保真审计完成：修复 classifier 关键词表漏 11 词、use_path_index 条件、rerank
  失败清空结果、selector 字节预算、source_filter 漏 .venv、formatter splitlines
  共 6 处；其余模块逐一对齐。
- 召回预算（借鉴 semble_rs 后实测校准）：default_top_k 50→120、per_query_top_k
  20→60。完全体 cc-switch 78.6%→83.9%（+9.6），flask 噪声带内持平，200 饱和。
- 已实测否定的思路（全部回退）：查询侧 camelCase 拆词变体（三种语义均无效）、
  按意图自适应 text_boost（-1.4~-4.3）。教训：RRF 融合对附加查询极敏感，
  启发式变体是噪声不是信号；预算类参数才是稳杠杆。
- error_handling 类仍是硬尾巴（5/10）。
- 网络检索补充实验（2026-09-05）：
  - TriviumDB SA-PPR 图扩散（expand_depth）：为同文件 chunk 建 "same_file" 双向边后
    开扩散 depth=2，flask nollm 70.4%→58.3%（-12 分）——扩散把整文件兄弟灌进候选池
    挤掉其他文件的真实命中，代码检索要多样性不要文件纵深。已回退默认 0，代码保留
    为 env 可选（TRIVIUM_EXPAND_DEPTH）。
  - HyDE 已整体清除（用户裁决不可接受：+1 分不值 3.8 倍延迟，且属查询侧
    叠加信号，与 RRF 融合的历史教训同源）。代码不留残迹，教训保留于此。
  - HyDE（现有 LLM 生成假设性代码片段→同嵌入器编码→额外变体）：首版串行
    27s/条（输出无上限+全串行）——慢的根因是 LLM 逐字解码，而 semble_rs 的
    "HyDE" 实为静态别名表投影（零 LLM 调用），二者不可比。优化：max_tokens 300
    + 与改写/召回 tokio 并发 → 17.2s（p50 12.5s），cc 85.0%（+1.02）。
    仍不值默认（+1 分换 3.8 倍延迟），保留为 RETRIEVAL_HYDE_ENABLED=1 可选。
- 本轮净产出：召回预算 default_top_k=120 / per_query_top_k=60（cc +9.6），
  其余三个召回思路全部实测否定并记录。
- 概念投影表 + 并行化 + 静态模型横评（2026-09-05 终轮）：
  - lexical.rs 概念→代码词汇投影（静态表，查询侧 BM25 通道）必须意图门控：
    仅 Feature/Overview/Compound 投影 → flask +1.3 / cc +2.0；无门控 -10.7
    （PATH 类查询的 BM25 是精确信号，别名 token 是噪声）。
  - 改写∥原查询召回并行化：tokio::join，RRF 首位权重约定保持（原查询列表居首）。
  - 本地神经嵌入（candle + vendored qwen3，EMBED_PROVIDER=local 新能力）：
    **EOS 池化位 bug 修复后 + 代码专用指令**：Qwen3-Embedding-0.6B 142.68
    (71.3%，距 API 仅 3.4 分)；F2LLM-v2-80M 138.72 (69.4%，官方 QA 指令；
    代码指令对它 -2.4，指令效果是模型相关的)。两者反超静态 potion 约 17 分。
    修复前（漏 EOS）0.6B 只有 106.69——lasttoken pooling 必须追加 EOS
    （官方 add_special_tokens=True 的显式等价），漏加则池化位错位、大幅劣化。
    指令覆盖：EMBED_LOCAL_QUERY_PROMPT。要点：candle 0.9.2 Metal 缺 rms-norm kernel 走 CPU+Accelerate；
    HF F2LLM 权重 key 无 model. 前缀需重映射；candle qwen3 的 clear_kv_cache
    是 pub(crate) → vendor 进来改 pub（独立查询间必须复位 KV，否则
    ConcatKvCache 拼接污染形状）。官方 query prompt 从
    config_sentence_transformers.json 自动读取。
  - 静态模型横评（flask nollm 同参）：potion-multilingual-128M 121.51 卫冕；
    6 个社区 M2V 候选全败（jina-v3-m2v-256 -5.7 / jina-v3-distilled -8.9 /
    F2LLM-v2-330M -13.7 / octen-8b -34.9 / jina-code -42.7 分）；
    Qwen3-8B-M2V-Entropy-RAG 加载失败（model2vec-rs 0.2.1 不支持 I8 权重）。
    教训：m2v 蒸馏质量参差，"教师模型大"与"静态化后召回质量"无关。
- Qwen3-Embedding query instruction（2026-09-05 追记）：官方指令感知机制实测
  nollm flask +1.08 / cc +2.35（完全体持平偏好）。修复了指令在凭据回落路径
  被硬编码 "" 丢弃的管道 bug；落为模型条件默认（Qwen3 系列自动启用，
  EMBED_QUERY_INSTRUCTION 显式覆盖，其他模型保持 Python 原默认空）。
  注意：指令只作用于查询侧，文档向量不变，无需重索引。

## MRL 维度扫描与批量实验（2026-09-05 终轮追记）

- Qwen3-0.6B MRL 扫描（flask nollm，各档独立索引）：320=141.24 / 32=131.70 /
  16=118.66 / 8=99.60 / 4=92.90。MRL 截断在前向之后 ⇒ 嵌入耗时恒定，
  索引仅 2.6MB ⇒ 精度换速度不成立（32 维为存储敏感场景的最低可用档）。
- 本地批量化：因果注意力 + 右 padding 数学等价于逐条前向（真实 token 不读 pad），
  但 candle 0.9.2 CPU 批量前向在掩码广播形状上 panic（utils.rs 广播快路径越界，
  上游 bug）→ 默认 EMBED_LOCAL_BATCH_SIZE=1；嵌入器加锁毒化恢复 + 每调用前复位 KV。
  本地推理真正提速路线：ort/ONNX（3-10×，需一次性导出）。
- 引擎选型结论：个人模式查询 2-4ms 非瓶颈，换引擎（usearch/lancedb/sqlite-vec）
  不动用户可感知延迟；大库（50 万 chunk+）再做选型 Benchmark。
- 教训汇总：本轮所有有效收益都来自"实现保真与参数校准"（EOS 池化位、指令管道、
  召回预算、意图门控投影），所有"引入新信号/换组件"的尝试（变体/扩散/HyDE/
  batch/MRL）要么无效要么有害。

## 权重精度扫描（2026-09-05 终轮）

Qwen3-0.6B + 代码指令（flask nollm，权重按目标精度舍入/逐行对称量化后反回
F32 计算 —— candle CPU 无 int 矩阵内核，本表衡量存储精度对质量的影响）：

| 精度 | 得分 | 权重存储 |
|---|---:|---:|
| f32 | 142.64 | 2273 MB |
| f16 | 142.68 | 1136 MB |
| bf16 | 142.97 | 1136 MB |
| int8 | 141.63 | 568 MB |
| int4 | 87.00（崩盘） | 284 MB |

结论：f16/bf16 半内存零损失（甚至 +0.3）；int8 -1 分换 75% 内存，值得；
int4 逐行对称量化崩盘（需 block-wise k-quant 才能救）。速度各档相同
（计算均 F32）——int 算力提速需 ort/GGUF 内核，candle CPU 不提供。
EMBED_LOCAL_PRECISION=f32|f16|bf16|int8|int4。

## llama.cpp 路线（2026-09-05 终轮追记）

llama-cpp-2 0.1.156（metal feature）+ vendored 嵌入器：
- 实现：GGUF（HF repo + 文件名或本地路径）、pooling 从 GGUF 元数据自动取
  （Qwen3-Embedding = LAST）、tokenize(AddBos::Never) + 显式追加 token_eos
  （与 candle 路线同一 EOS 约定）、ctx 按调用创建（避免自引用生命周期）、
  L2 + MRL 截断、官方 query prompt 回落链（GGUF 仓 → 去后缀原仓）。
- 实测（flask nollm）：Q8_0 137.53 (68.8%) ≈ f16 137.38 (68.7%) —— 同分证明
  -5 分（vs candle F32 142.68）是运行时差异（llama.cpp tokenizer/Metal 数值）
  而非量化。速度：单条 0.15s（Metal，candle CPU 的 23×）、全量 173s（2.4×）。
- 两路线并存：EMBED_LOCAL_BACKEND=candle（默认，质量）| llama（速度）。
- 坑：GGUF 文件名大小写（andquant 仓是小写文件名；官方 Qwen/Qwen3-Embedding-
  0.6B-GGUF 用标准命名）；EMBED_LOCAL_QUERY_PROMPT 带内嵌换行时 shell 多行
  env 赋值的续接符易被脚本编辑吃掉导致 env 断裂（指纹保护正确拦截了误配置）。

## 批量化结论与防护（2026-09-05 终轮补）

- candle 批量前向（b>1）上游 bug 双重表现：batch=8 CPU panic（广播越界）、
  batch=50 静默错值（flask 141→15 分）。等价性自检（批量 vs 逐条，max_diff
  阈值 1e-3）+ catch_unwind 拦 panic，任一触发即拒绝启动并提示 batch=1。
- 防护验证：EMBED_LOCAL_BATCH_SIZE=50 → 装配失败并提示回退 ✓
- F2LLM-v2-0.6B Q8_0（llama 路线）flask nollm 137.46 (68.7%) —— 与
  Qwen3-0.6B Q8_0 (137.53) 同档，llama.cpp 路线的质量与速度结论不变。

## voyage-4-large 横测（2026-09-06）

flask 142.06 (71.0%) / cc-switch 102.08 (51.0%)：flask 低于 Qwen3-4B 4.5 分、
cc 反超 2.4 分（cross_language 10/10）。要点：① Voyage API 契约与 OpenAI 差异
（encoding_format 仅 base64、dimensions→output_dimension、input_type 必按
query/document 注入）已在嵌入器按模型名自动适配；② output_dimension 缺省返回
1024（MRL 档 [256,512,1024,2048]，原生 2048 需显式传）；③ 嵌入器错误传播修复：
批量任务失败此前被统一吞成 "semaphore closed"，现真实 4xx 上抛并携带 API 实际
返回维度。教训：测新供应商前先打一发真实请求看契约差异，别信"OpenAI 兼容"的
泛泛说法。

## Qwen3-8B 横测（2026-09-06，Nebius API）

flask 141.15 (1024 维) / 141.89 (4096 维) —— MRL 截断无感；cc-switch 104.87
(52.4%) @4096。对比 Qwen3-4B（SiliconFlow，1024）：flask -4~-5、cc +5.2，
总分打平、分布互补（与 voyage 同模式：跨语言重仓利好更强多语言模型）。
8B 单价 ~2×、速度 -20%。GPT 预测"高概率超过 4B"只对了一半。
工程注意：Nebius 单条嵌入延迟 5-8s（网络+8B 推理），索引/查询都比
SiliconFlow 4B 慢 20-100%；4B 既有 instruction 管道对 8B 同样生效
（模型名含 Qwen3-Embedding 的条件默认）。

## 自训静态模型 ft-sprint 横测（2026-09-06）

用户自训 80M 中英 model2vec（/Users/wxy/code/ft-sprint，EMBED_STATIC_MODEL_DIR
加载）：flask 128.41 (64.2%) / cc-switch 75.59 (37.8%)——双仓反超
potion-multilingual-128M（121.51/71.70，同代码同参重跑 cc 基线）+6.9/+3.9。
静态查表速度（flask 全量 3s / cc 35s）。首个在静态路线上击败
potion-multilingual 的模型（此前 6 个社区 M2V 候选全败）。
