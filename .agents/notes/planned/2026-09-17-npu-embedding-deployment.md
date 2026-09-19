# AMD 8845HS NPU 部署 Embedding 模型（支持 EOS/last-token pooling）

**硬件**：Ryzen 7 8845HS (Hawk Point, XDNA1, PCI 1022:1502, 16 TOPS, 4 列可用)
**内存**：64 GB
**目标**：在 NPU 上跑 Qwen3-Embedding-0.6B/4B ONNX（last-token pooling 烘焙进图），通过 OpenAI `/v1/embeddings` API 对外服务

## 优先级路径

```
阶段 1：xdna-driver + VitisAI EP（验证 decoder 算子支持）
  ↓ 失败
阶段 2：IREE + MLIR-AIE（ryzen-npu-linux，自编译到 NPU）
  ↓ 失败
阶段 3：Radeon 780M iGPU + MIGraphX/ROCm EP（GPU 加速）
```

---

## 阶段 1：xdna-driver + ONNX Runtime VitisAI EP

### 1.1 确认内核和 PCI 设备

```bash
# 内核 >= 6.10 才有 amdxdna 支持
uname -r

# 确认 NPU PCI 设备存在
lspci -nn | grep -i "1502\|17f0\|XDNA\|NPU"
# 预期输出包含 1022:1502（Hawk Point NPU）

# 确认 IOMMU 支持
grep -c AMD_IOMMU /boot/config-$(uname -r) 2>/dev/null || echo "需检查 IOMMU"
```

如果内核 < 6.10，升级：
```bash
# Ubuntu 24.04 HWE
sudo apt update && sudo apt install --install-recommends linux-generic-hwe-24.04
sudo reboot
```

### 1.2 编译安装 xdna-driver + XRT

```bash
# 安装依赖
sudo apt install -y git git-lfs build-essential dkms libboost-filesystem1.74.0

# 克隆（含子模块）
git clone git@github.com:amd/xdna-driver.git
cd xdna-driver
git lfs install
git submodule update --init --recursive

# 编译 XRT base
cd xrt/build
./build.sh -npu -opt

# 安装 XRT
sudo apt install -y ./Release/xrt_*-base.deb
sudo apt install -y ./Release/xrt_*-base-dev.deb
sudo apt install -y ./Release/xrt_*-npu.deb

# 编译 XDNA driver + plugin
cd ../../build
./build.sh -release

# 安装 XDNA plugin（含 amdxdna.ko DKMS 驱动 + NPU 固件）
sudo apt install -y ./Release/xrt_plugin.*amdxdna.deb

# 设置 memlock（NPU 访问需要）
sudo mkdir -p /etc/security/limits.d
sudo tee /etc/security/limits.d/99-amdxdna.conf > /dev/null << 'EOF'
* soft memlock unlimited
* hard memlock unlimited
EOF

# 重新登录或重启让 memlock 生效
```

### 1.3 验证 NPU 可用

```bash
source /opt/xilinx/xrt/setup.sh

# 查看 NPU 设备
xrt-smi examine
# 预期：看到 NPU 设备（名字可能是 "NPU" 或 "RyzenAI-npu1"，架构 aie2p）

# 运行验证
xrt-smi validate
# 预期：测试通过
```

如果 `xrt-smi examine` 不显示 NPU：
```bash
# 手动加载驱动
sudo modprobe amdxdna
dmesg | tail -20 | grep -i xdna

# 检查固件
ls /usr/lib/firmware/amdnpu/
```

### 1.4 安装 ONNX Runtime + VitisAI EP

VitisAI EP 需要配套 Ryzen AI Software 的编译器和配置。但 8845HS (Hawk Point) **不在** AMD 官方 Ryzen AI Software 1.8 支持列表（只支持 STX/KRK）。

**尝试方案 A：直接装 Ryzen AI Software 1.8（可能检测不通过）**

```bash
# 下载 Ryzen AI 1.8
# 从 https://account.amd.com/en/forms/downloads/ryzenai-eula-public-xef.html?filename=ryzen_ai-1.8.0.tgz
# 下载后：
mkdir -p ~/ryzen_ai-1.8.0 && cd ~/ryzen_ai-1.8.0
tar -xvzf ~/Downloads/ryzen_ai-1.8.0.tgz
./install_ryzen_ai.sh -a yes -p ~/ryzen_ai-venv
source ~/ryzen_ai-venv/bin/activate
export LD_LIBRARY_PATH=/lib/x86_64-linux-gnu:$RYZEN_AI_INSTALLATION_PATH/onnxruntime/lib/:$LD_LIBRARY_PATH
source /opt/xilinx/xrt/setup.sh
```

**尝试方案 B：从 PyPI 装 onnxruntime-vitisai（独立于 Ryzen AI Software）**

```bash
pip install onnxruntime-vitisai
# 或者直接
pip install onnxruntime
# 然后单独编译 VitisAI EP（从源码）
```

> **关键判断**：如果 `VitisAIExecutionProvider` 无法加载或检测不到 NPU，
> 说明 Hawk Point 不被 VitisAI EP 编译器支持，**直接跳到阶段 2**。

### 1.5 准备 ONNX 模型（last-token pooling baked in）

```bash
pip install onnxruntime huggingface_hub

# 下载 Qwen3-Embedding-0.6B ONNX（首选，因为 0.6B 更小更快验证）
# 0.6B 没有现成 ONNX，需要自己导出
# 先用 4B ONNX 做验证（有现成的）

# 下载已烘焙 last-token pooling 的 ONNX
huggingface-cli download aryeh-tiktinsky/Qwen3-Embedding-4B-ONNX \
  --local-dir ~/onnx-models/Qwen3-Embedding-4B-ONNX

# 如果 4B 太大，自己导出 0.6B（见下文）
```

**自己导出 Qwen3-Embedding-0.6B ONNX（last-token pooling baked in）**：

```python
# export_qwen3_embed_onnx.py
import torch
from transformers import AutoTokenizer, AutoModel

model_id = "Qwen/Qwen3-Embedding-0.6B"
output_dir = "~/onnx-models/Qwen3-Embedding-0.6B-ONNX"

tokenizer = AutoTokenizer.from_pretrained(model_id, padding_side="left")
model = AutoModel.from_pretrained(model_id, torch_dtype=torch.float32)
model.eval()

class Qwen3EmbeddingWithPooling(torch.nn.Module):
    """包装器：Qwen3 模型 + last-token pooling 烘焙进 ONNX 图"""
    def __init__(self, model):
        super().__init__()
        self.model = model

    def forward(self, input_ids, attention_mask):
        outputs = self.model(input_ids=input_ids, attention_mask=attention_mask)
        last_hidden = outputs.last_hidden_state  # (batch, seq, hidden)
        # last-token pooling：取 attention_mask 最后一个 1 的位置
        # left_padding 时直接取 [:, -1]
        # right_padding 时需要用 attention_mask.sum(dim=1) - 1
        sequence_lengths = attention_mask.sum(dim=1) - 1
        batch_size = last_hidden.shape[0]
        pooled = last_hidden[torch.arange(batch_size, device=last_hidden.device), sequence_lengths]
        # unsqueeze 到 (batch, 1, hidden)——兼容 Vespa/SentenceTransformer 的 mean pooling（单元素 no-op）
        return pooled.unsqueeze(1)

wrapped = Qwen3EmbeddingWithPooling(model)

# 导出
dummy_ids = torch.randint(0, 100, (1, 16), dtype=torch.long)
dummy_mask = torch.ones(1, 16, dtype=torch.long)

torch.onnx.export(
    wrapped,
    (dummy_ids, dummy_mask),
    f"{output_dir}/model.onnx",
    input_names=["input_ids", "attention_mask"],
    output_names=["pooled_embedding"],
    dynamic_axes={
        "input_ids": {0: "batch", 1: "seq"},
        "attention_mask": {0: "batch", 1: "seq"},
        "pooled_embedding": {0: "batch"},
    },
    opset_version=17,
    do_constant_folding=True,
)
print(f"Exported to {output_dir}/model.onnx")
```

### 1.6 测试 VitisAI EP 算子支持

```python
# test_vitisai_ep.py
import onnxruntime as ort
import numpy as np
import json

model_path = "~/onnx-models/Qwen3-Embedding-4B-ONNX/model.onnx"  # 或 0.6B

# 先用 CPU EP 确认模型可跑
print("=== CPU EP ===")
sess_cpu = ort.InferenceSession(model_path, providers=["CPUExecutionProvider"])
input_ids = np.array([[1, 2, 3, 4, 5, 2]], dtype=np.int64)
attention_mask = np.array([[1, 1, 1, 1, 1, 0]], dtype=np.int64)
out_cpu = sess_cpu.run(None, {"input_ids": input_ids, "attention_mask": attention_mask})
print(f"CPU output shape: {out_cpu[0].shape}")

# 尝试 VitisAI EP
print("\n=== VitisAI EP ===")
try:
    # 需要配置文件（从 Ryzen AI Software 获取或自建）
    # 先试不带配置
    provider_opts = {
        "cache_dir": "/tmp/npu_cache",
        "cache_key": "qwen3-embed-test",
    }
    sess_npu = ort.InferenceSession(
        model_path,
        providers=[("VitisAIExecutionProvider", provider_opts)],
    )
    out_npu = sess_npu.run(None, {"input_ids": input_ids, "attention_mask": attention_mask})
    print(f"NPU output shape: {out_npu[0].shape}")

    # 验证结果一致性
    cos_sim = np.dot(out_cpu[0].flatten(), out_npu[0].flatten()) / (
        np.linalg.norm(out_cpu[0].flatten()) * np.linalg.norm(out_npu[0].flatten())
    )
    print(f"Cosine similarity CPU vs NPU: {cos_sim:.6f}")

    # 检查哪些算子在 NPU 上
    # 通过 session log 确认
    print("\nVitisAI EP loaded successfully!")

except Exception as e:
    print(f"VitisAI EP FAILED: {e}")
    print("→ Hawk Point NPU 可能不被 VitisAI EP 支持")
    print("→ 进入阶段 2（IREE + MLIR-AIE）")
```

**开启详细日志**确认 NPU 算子分配：

```python
import onnxruntime as ort
# 设置日志级别
sess_opts = ort.SessionOptions()
sess_opts.log_severity_level = 1  # 1=verbose
sess_opts.log_verbosity_level = 1

# 查看 EP 分配详情
# VitisAI EP 编译时会输出哪些 subgraph 被分配到 NPU
```

或用环境变量：
```bash
ORT_LOGGING_LEVEL=1 python test_vitisai_ep.py 2>&1 | grep -i "vitis\|partition\|npu\|fallback"
```

### 1.7 阶段 1 判定

| 结果 | 下一步 |
|---|---|
| VitisAI EP 加载成功 + NPU 有算子分配 + 结果 cos > 0.99 | **成功！** 进入 1.8 量化 + 部署 |
| VitisAI EP 加载成功但**全部 fallback 到 CPU** | VitisAI 编译器不支持 Qwen3 的算子 → **阶段 2** |
| VitisAI EP 加载失败 / 找不到 provider | Hawk Point 不在支持列表 → **阶段 2** |

### 1.8 量化 + 部署（如果阶段 1 成功）

```bash
# 安装 AMD Quark
pip install amd-quark

# INT8 量化（NPU 最佳格式）
python3 << 'EOF'
import quark.onnx.quantization.config as config
from quark.onnx import ModelQuantizer

model_path = "~/onnx-models/Qwen3-Embedding-0.6B-ONNX/model.onnx"
quant_config = config.DefaultStaticConfig(
    quant_format=config.QuantFormat.QDQ,
    activation_type=config.QuantType.QUInt8,
    weight_type=config.QuantType.QInt8,
)
quantizer = ModelQuantizer(quant_config)
quantizer.quantize(model_path, output_dir="~/onnx-models/Qwen3-Embedding-0.6B-ONNX-INT8")
EOF
```

---

## 阶段 2：IREE + MLIR-AIE（ryzen-npu-linux）

VitisAI EP 不支持 Hawk Point 的 decoder 算子时，走 IREE 路径——直接将 ONNX 编译成 NPU 机器码，绕过 VitisAI 编译器限制。

### 2.1 前提

- 阶段 1 的 xdna-driver + XRT 已安装且 `xrt-smi validate` 通过
- 30-60 GB 磁盘空间
- 16+ GB RAM

### 2.2 克隆 ryzen-npu-linux

```bash
git clone https://github.com/Jonas-Augustinus-Linus/ryzen-npu-linux.git
cd ryzen-npu-linux
git checkout v1.1.0  # 最新 release
```

### 2.3 检测 NPU 类型

```bash
./scripts/detect-npu.sh
# 预期输出：
#   NPU detected: RyzenAI-npu1
#   Usable columns: 4
#   IREE target: npu1_4col
```

如果输出 `npu1_4col`，继续。如果检测不到，手动设置：
```bash
export TARGET_DEVICE=npu1_4col
```

### 2.4 启用 NPU

```bash
# 检查驱动、用户组、memlock
./scripts/check-npu.sh

# 如果需要，启用 NPU（安装驱动、设用户组、设 memlock）
# ⚠️ 先阅读脚本内容再执行
cat ./scripts/enable-npu.sh
./scripts/enable-npu.sh
```

### 2.5 构建 IREE + MLIR-AIE 工具链

```bash
# 完整构建（需要 30-60 分钟）
./scripts/build.sh
# 这会：
#   1. 下载 IREE 源码 + mlir-aie + LLVM-AIE
#   2. 编译 IREE 编译器和运行时
#   3. 编译 mlir-aie 工具

# 验证构建
./scripts/verify-stack.sh --quick
# 预期：CPU-reference 匹配通过（i32 和 bf16）
```

### 2.6 编译 ONNX 模型到 NPU

```bash
# 设置环境
source scripts/env.sh  # 或按项目文档设置 IREE 相关路径

# 将 Qwen3-Embedding ONNX 编译为 NPU 可执行文件
# IREE 使用两步编译：
#   1. ONNX → IREE VM bytecode（mlir 级别优化）
#   2. IREE bytecode → NPU 可执行（mlir-aie 编译到 aie2p tile array）

# Step 1: ONNX → IREE bytecode
iree-compile \
  ~/onnx-models/Qwen3-Embedding-0.6B-ONNX/model.onnx \
  --iree-hal-target-backends=rocm-ext \
  --iree-vmv8-emit-bytecode \
  -o /tmp/qwen3-embed-0.6b.vmfb

# Step 2: 部署到 NPU（通过 XRT SHIM）
# IREE 运行时通过 XRT SHIM 与 NPU 通信
iree-run-module \
  --module=/tmp/qwen3-embed-0.6b.vmfb \
  --device=xrt \
  --function=forward \
  --input="1x16xi64=..." # dummy input
```

> **注意**：IREE 对 ONNX 的 decoder 架构（causal attention、RoPE 等）的支持
> 仍在快速演进中。Qwen3-Embedding 的 GQA + RoPE + RMSNorm + SwiGLU
> 组合是否能完整编译到 `npu1_4col`（4 列 tile）**需要实际验证**。
> 如果编译失败，看具体报错：
> - **tile 资源不足**：0.6B 模型小，4 列可能够；4B/8B 可能不够
> - **算子不支持**：某些 ONNX 算子（如 dynamic gather、where）可能需要拆分

### 2.7 阶段 2 判定

| 结果 | 下一步 |
|---|---|
| 编译成功 + NPU 推理结果正确 | **成功！** 包装成 API 服务 |
| 编译失败（算子不支持 / tile 不够） | **阶段 3**（iGPU） |

### 2.8 包装 API 服务（如果成功）

```python
# iree_embed_server.py
import json, numpy as np
from fastapi import FastAPI, Header, HTTPException
from pydantic import BaseModel
import iree.runtime as rt
from tokenizers import Tokenizer

app = FastAPI()

# 加载 IREE 模块
config = rt.Config("xrt")  # XRT 后端 = NPU
vm_instance = rt.VmInstance()
module = rt.VmModule.from_file(vm_instance, "/tmp/qwen3-embed-0.6b.vmfb")
# ... (IREE runtime API 初始化)

tokenizer = Tokenizer.from_file("~/onnx-models/tokenizer.json")

class EmbedRequest(BaseModel):
    input: str | list[str]
    model: str = "qwen3-embedding-0.6b"

@app.post("/v1/embeddings")
async def embeddings(req: EmbedRequest):
    texts = req.input if isinstance(req.input, list) else [req.input]
    results = []
    for text in texts:
        enc = tokenizer.encode(text)
        ids = enc.ids
        mask = [1] * len(ids)
        # 调用 IREE NPU 推理
        # ...
        embedding = np.array(result).flatten().tolist()
        results.append(embedding)
    return {"data": [{"embedding": e, "index": i} for i, e in enumerate(results)], "model": req.model}
```

---

## 阶段 3：Radeon 780M iGPU + MIGraphX/ROCm EP

NPU 路不通时，用 iGPU。Radeon 780M (RDNA3, GC 11.0.0) 通过 ROCm + MIGraphX EP 跑 ONNX embedding。

### 3.1 安装 ROCm 驱动

```bash
# 添加 AMD ROCm apt 源
sudo apt install -y linux-headers-$(uname -r)
wget https://repo.radeon.com/amdgpu-install/latest/ubuntu/focal/amdgpu-install_6.3.60100-1_all.deb
sudo apt install -y ./amdgpu-install_6.3.60100-1_all.deb
sudo amdgpu-install --usecase=rocm

# 添加用户到 render/video 组
sudo usermod -aG render,video $USER

# 验证
rocm-smi
# 预期：看到 GPU 0 (Radeon 780M)

# 验证 MIGraphX
/opt/rocm/bin/migraphx-driver perf --test
```

### 3.2 安装 ONNX Runtime + MIGraphX EP

```bash
# 方案 A：pip 安装预编译 wheel
pip install onnxruntime-migraphx -f https://repo.radeon.com/rocm/manylinux/rocm-rel-7.2.1/

# 验证
python3 -c "import onnxruntime as ort; print(ort.get_available_providers())"
# 预期：['MIGraphXExecutionProvider', 'ROCMExecutionProvider', 'CPUExecutionProvider']
```

### 3.3 跑 Qwen3-Embedding ONNX

```python
# test_migraphx_ep.py
import onnxruntime as ort
import numpy as np

model_path = "~/onnx-models/Qwen3-Embedding-0.6B-ONNX/model.onnx"

# MIGraphX EP（GPU 加速）
sess = ort.InferenceSession(
    model_path,
    providers=[("MIGraphXExecutionProvider", {"device_id": 0})],
)
print(f"Providers: {sess.get_providers()}")

# 测试推理
input_ids = np.array([[1, 100, 200, 151645]], dtype=np.int64)
attention_mask = np.array([[1, 1, 1, 1]], dtype=np.int64)
out = sess.run(None, {"input_ids": input_ids, "attention_mask": attention_mask})
print(f"Output shape: {out[0].shape}")
print(f"First 5 values: {out[0].flatten()[:5]}")
```

### 3.4 量化（FP16 就够了，iGPU 原生支持）

```python
# 0.6B FP16 ONNX（比 FP32 快 ~2x，质量损失极小）
import torch
from transformers import AutoModel

model = AutoModel.from_pretrained("Qwen/Qwen3-Embedding-0.6B", torch_dtype=torch.float16)
# 导出 FP16 ONNX（同 1.5 的导出脚本，改 dtype=torch.float16）
```

### 3.5 部署 API 服务

```python
# migraphx_embed_server.py — OpenAI /v1/embeddings 兼容
import numpy as np
import onnxruntime as ort
from tokenizers import Tokenizer
from fastapi import FastAPI
from pydantic import BaseModel

app = FastAPI()

MODEL_PATH = "~/onnx-models/Qwen3-Embedding-0.6B-ONNX-FP16/model.onnx"
TOKENIZER_PATH = "~/onnx-models/Qwen3-Embedding-0.6B-ONNX-FP16/tokenizer.json"
EOS_TOKEN_ID = 151645
INSTRUCTION = "Given a code retrieval query, retrieve the most relevant code snippets or files that directly implement, explain, or help answer the query."

sess = ort.InferenceSession(
    MODEL_PATH,
    providers=[("MIGraphXExecutionProvider", {"device_id": 0})],
)
tokenizer = Tokenizer.from_file(TOKENIZER_PATH)
tokenizer.with_padding(None)

class EmbedRequest(BaseModel):
    input: str | list[str]
    model: str = "qwen3-embedding-0.6b"

def encode(text: str, is_query: bool = False) -> list[float]:
    if is_query and INSTRUCTION:
        text = f"{INSTRUCTION}{text}"
    enc = tokenizer.encode(text)
    ids = enc.ids
    if ids[-1] != EOS_TOKEN_ID:
        ids.append(EOS_TOKEN_ID)
    mask = [1] * len(ids)
    max_len = max(len(ids), 1)
    ids_padded = ids + [0] * (max_len - len(ids))
    mask_padded = mask + [0] * (max_len - len(mask))

    input_ids = np.array([ids_padded], dtype=np.int64)
    attention_mask = np.array([mask_padded], dtype=np.int64)

    out = sess.run(None, {"input_ids": input_ids, "attention_mask": attention_mask})
    # out[0]: (batch, 1, hidden) — last-token pooling 已烘焙
    emb = out[0].squeeze()  # (hidden,)
    # L2 归一化
    norm = np.linalg.norm(emb)
    if norm > 0:
        emb = emb / norm
    return emb.tolist()

@app.post("/v1/embeddings")
async def embeddings(req: EmbedRequest):
    texts = req.input if isinstance(req.input, list) else [req.input]
    data = [{"embedding": encode(t, is_query=True), "index": i} for i, t in enumerate(texts)]
    return {"data": data, "model": req.model, "object": "list", "usage": {"prompt_tokens": 0, "total_tokens": 0}}

# 启动：uvicorn migraphx_embed_server:app --host 0.0.0.0 --port 8980
```

---

## 预期结果和注意事项

### 阶段成功率预估

| 阶段 | 可能性 | 原因 |
|---|---|---|
| 阶段 1 VitisAI EP | ~20% | Hawk Point 不在官方支持列表；decoder 架构图可能无法完整编译到 NPU |
| 阶段 2 IREE | ~40% | 社区已有 Phoenix (XDNA1) 实机验证，Hawk Point 身份已映射；但 Qwen3 的 GQA+RoPE 复杂算子在 4 列 tile 上可能资源不足 |
| 阶段 3 iGPU | ~95% | MIGraphX 对 RDNA3 支持成熟；ONNX decoder 模型在 GPU 上运行无特殊限制 |

### 关键注意事项

1. **固件/驱动版本匹配**：XDNA NPU 对固件版本极其敏感，混用会导致命令超时/中断。始终用 xdna-driver 仓库编译的固件。

2. **memlock 必须设为 unlimited**：NPU BO（Buffer Object）分配依赖锁定内存，不设会报 `failed to allocate BO`。

3. **量化精度验证**：INT8 量化后必须跑 cosine similarity 验证（与 FP32 参考对比，cos > 0.995 为安全线）。

4. **last-token pooling 正确性**：ONNX 模型中 pooling 已烘焙，但需要验证 `attention_mask` 处理是否正确——特别是右 padding 序列（文档）和左 padding 序列（query）的行为差异。

5. **Qwen3-Embedding-0.6B vs 4B**：0.6B 在 NPU 上更可能装下（4 列 tile 资源有限）；4B/8B 大概率不行。iGPU 路径则无此限制。

6. **OCE 集成**：API 服务启动后，在 OCE 的 `.env` 中设置：
   ```
   EMBED_ENDPOINT=http://localhost:8980/v1/embeddings
   EMBED_MODEL=qwen3-embedding-0.6b
   EMBED_API_KEY=（如果设了）
   EMBED_DIMENSIONS=1024
   EMBED_QUERY_INSTRUCTION=Given a code retrieval query, retrieve the most relevant code snippets or files that directly implement, explain, or help answer the query.
   ```
