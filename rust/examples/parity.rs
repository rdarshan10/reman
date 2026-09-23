use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};
fn main() -> anyhow::Result<()> {
    let db = rusqlite::Connection::open(dirs::home_dir().unwrap().join(".reman/reman.db"))?;
    let mut st = db.prepare("SELECT c.cmd_text, v.vec FROM commands c JOIN command_vec v ON v.command_id=c.id ORDER BY c.id DESC LIMIT 200")?;
    let rows: Vec<(String, Vec<u8>)> = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
    let t = std::time::Instant::now();
    let mut m = TextEmbedding::try_new(TextInitOptions::new(EmbeddingModel::BGESmallENV15Q)
        .with_cache_dir(dirs::home_dir().unwrap().join(".reman/models")).with_show_download_progress(false))?;
    println!("load {:?}", t.elapsed());
    let texts: Vec<&str> = rows.iter().map(|r| r.0.as_str()).collect();
    let t = std::time::Instant::now();
    let vs = m.embed(&texts, Some(64))?;
    println!("batch200 {:?}", t.elapsed());
    let (mut min, mut sum) = (1f32, 0f32);
    for ((cmd, blob), v) in rows.iter().zip(&vs) {
        let s: Vec<f32> = blob.chunks_exact(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect();
        let (d, na, nb) = s.iter().zip(v).fold((0f32,0f32,0f32), |(d,a,b),(x,y)| (d+x*y, a+x*x, b+y*y));
        let c = d / (na.sqrt()*nb.sqrt()); sum += c;
        if c < min { min = c; }
        if c < 0.999 { println!("low {c:.4} {cmd:?}"); }
    }
    println!("cos min {min:.5} mean {:.5}", sum / rows.len() as f32);
    for q in ["run database migrations", "a"] { let t = std::time::Instant::now(); m.embed(&[q], None)?; println!("single {:?}", t.elapsed()); }
    Ok(())
}
