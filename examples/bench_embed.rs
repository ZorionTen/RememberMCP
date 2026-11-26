use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use std::time::Instant;

fn realistic_texts() -> Vec<String> {
    let base = "The deployment pipeline uses a blue-green strategy on AWS ECS with health checks and rolling traffic shift. ";
    let mut out = Vec::new();
    for (i, chars) in [500usize, 1000, 2000, 2600]
        .iter()
        .cycle()
        .take(48)
        .enumerate()
    {
        let mut s = format!("chunk {i}: ");
        while s.len() < *chars {
            s.push_str(base);
        }
        s.truncate(*chars);
        out.push(s);
    }
    out
}

fn bench(label: &str, model: EmbeddingModel, max_len: usize, texts: &[String]) {
    let t0 = Instant::now();
    let m = match TextEmbedding::try_new(
        InitOptions::new(model)
            .with_max_length(max_len)
            .with_show_download_progress(false),
    ) {
        Ok(m) => m,
        Err(e) => {
            println!("{label}: INIT FAILED: {e}");
            return;
        }
    };
    let init = t0.elapsed();
    let t1 = Instant::now();
    let v = m.embed(texts.to_vec(), Some(32)).expect("embed");
    let el = t1.elapsed();
    println!(
        "{label}: init={init:?} embed {} texts={el:?} => {:.1}ms/text dims={}",
        texts.len(),
        el.as_secs_f64() * 1000.0 / texts.len() as f64,
        v[0].len()
    );
}

fn main() {
    if std::env::var_os("FASTEMBED_CACHE_DIR").is_none() {
        std::env::set_var(
            "FASTEMBED_CACHE_DIR",
            "/home/zorionten/.cache/remember-mcp/fastembed",
        );
    }
    let texts = realistic_texts();
    let chars: usize = texts.iter().map(|t| t.len()).sum();
    println!("{} texts, avg {} chars", texts.len(), chars / texts.len());
    bench("fp32 maxlen512", EmbeddingModel::BGESmallENV15, 512, &texts);
    bench("fp32 maxlen256", EmbeddingModel::BGESmallENV15, 256, &texts);
    bench("fp32 maxlen192", EmbeddingModel::BGESmallENV15, 192, &texts);
}
