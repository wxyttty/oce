//! 嵌入模型质量对比探针：中文业务查询 vs 代码块（正例/负例）的余弦区分度。
//! 用法：cargo run -p oce-infra --example m2v_quality_probe -- <model_id> [model_id ...]

use oce_core::search::Embedder;

/// 用户实测失败的查询（中文业务术语）
const QUERY_A: &str = "三明代发：调拨单打印与发货汇总单打印的期号(期数)逻辑，期号如何生成、写入、按期号查询汇总数据";
/// 用户实测成功的表述（贴近字段名）
const QUERY_B: &str = "package count remainder calcPkQty proxyPackageQty print form";

/// 正例：代发打印模板片段（模拟 ProxySendPrintForm.vue 内容）
const POSITIVE: &str = "File: jcfx-admin/src/views/wl/proxysend/ProxySendPrintForm.vue\n\n<template>\n  <el-table>\n    <el-table-column label=\"期号\" prop=\"catalogNo\" />\n    <el-table-column label=\"整包数\" prop=\"proxyPackageQty\" />\n    <el-table-column label=\"零头件数\">\n      wholePkQty = proxyPackageQty - 1\n      lastRemainVolQty = Math.round((calcPkQty % 1) * volPerPack)\n    </el-table-column>\n  </el-table>\n</template>\n// 三明代发 调拨单打印 期号显示";

/// 负例：框架样板（用户实测返回的无关内容）
const NEGATIVE: &str = "File: jcfx-project/jcfx-framework/jcfx-spring-boot-starter-protection/src/main/java/com/fjxhfx/jcfx/framework/idempotent/package-info.java\n\n/**\n * 幂等组件，参考 https://github.com/it4alla/idempotent 项目实现\n * 实现原理是，相同参数的方法，一段时间内，有且仅能执行一次。\n */\npackage com.fjxhfx.jcfx.framework.idempotent;";

fn cos(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    dot / (na * nb)
}

fn probe(model_id: &str) {
    println!("=== {model_id} ===");
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let mut s = oce_infra::settings::EmbeddingSettings::from_env();
        s.static_model = Some(model_id.to_string());
        let e = match oce_infra::static_embed::StaticEmbedder::load(&s) {
            Ok(e) => e,
            Err(err) => {
                println!("  load failed: {err}");
                return;
            }
        };
        println!("  dim={}", e.dim());
        let qa = e.embed_query(QUERY_A).await.unwrap();
        let qb = e.embed_query(QUERY_B).await.unwrap();
        let chunks = e.embed_documents(vec![POSITIVE.into(), NEGATIVE.into()]).await.unwrap();
        let (pos, neg) = (&chunks[0], &chunks[1]);
        println!("  queryA(中文业务) -> 正例 {:.4} | 负例 {:.4} | margin {:+.4}",
            cos(&qa, pos), cos(&qa, neg), cos(&qa, pos) - cos(&qa, neg));
        println!("  queryB(字段名)   -> 正例 {:.4} | 负例 {:.4} | margin {:+.4}",
            cos(&qb, pos), cos(&qb, neg), cos(&qb, pos) - cos(&qb, neg));
        println!("  正例 vs 负例 chunk 相似度 {:.4}（越接近 1 = 全库向量趋同）", cos(pos, neg));
    });
}

fn main() {
    let models: Vec<String> = std::env::args().skip(1).collect();
    let models = if models.is_empty() {
        vec!["minishlab/potion-base-8M".into()]
    } else {
        models
    };
    for m in models {
        probe(&m);
    }
}
