use anyhow::{anyhow, Context, Result};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36";

fn client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(USER_AGENT)
        .build()
        .expect("构建 HTTP 客户端失败")
}

pub fn get_text(url: &str) -> Result<String> {
    let resp = client()
        .get(url)
        .send()
        .with_context(|| format!("请求失败: {url}"))?;
    if !resp.status().is_success() {
        return Err(anyhow!("HTTP 状态码 {}: {url}", resp.status()));
    }
    resp.text().with_context(|| format!("读取响应失败: {url}"))
}

pub fn get_bytes(url: &str) -> Result<Vec<u8>> {
    let resp = client()
        .get(url)
        .send()
        .with_context(|| format!("请求失败: {url}"))?;
    if !resp.status().is_success() {
        return Err(anyhow!("HTTP 状态码 {}: {url}", resp.status()));
    }
    let bytes = resp
        .bytes()
        .with_context(|| format!("读取响应失败: {url}"))?;
    Ok(bytes.to_vec())
}

type FetchResults = Arc<Mutex<Vec<Option<Result<Vec<u8>>>>>>;

pub fn fetch_many(urls: &[String], max_workers: usize, sleep_ms: u64) -> Vec<Result<Vec<u8>>> {
    let n = urls.len();
    if n == 0 {
        return Vec::new();
    }

    let urls = Arc::new(urls.to_vec());
    let results: FetchResults = Arc::new(Mutex::new((0..n).map(|_| None).collect()));
    let (tx, rx) = mpsc::channel::<usize>();
    let rx = Arc::new(Mutex::new(rx));

    let worker_count = max_workers.clamp(1, n);
    let mut handles = Vec::with_capacity(worker_count);
    for _ in 0..worker_count {
        let rx = Arc::clone(&rx);
        let results = Arc::clone(&results);
        let urls = Arc::clone(&urls);
        handles.push(thread::spawn(move || {
            let mut last = Instant::now() - Duration::from_millis(sleep_ms + 1);
            loop {
                let idx = {
                    let guard = rx.lock().unwrap();
                    match guard.recv() {
                        Ok(i) => i,
                        Err(_) => break,
                    }
                };
                if sleep_ms > 0 {
                    let elapsed = last.elapsed();
                    if elapsed < Duration::from_millis(sleep_ms) {
                        thread::sleep(Duration::from_millis(sleep_ms) - elapsed);
                    }
                }
                let res = get_bytes(&urls[idx]);
                last = Instant::now();
                results.lock().unwrap()[idx] = Some(res);
            }
        }));
    }

    for i in 0..n {
        let _ = tx.send(i);
    }
    drop(tx);

    for h in handles {
        let _ = h.join();
    }

    let mut guard = results.lock().unwrap();
    std::mem::take(&mut *guard)
        .into_iter()
        .map(|o| o.expect("结果缺失"))
        .collect()
}
