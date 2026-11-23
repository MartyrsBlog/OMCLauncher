//! 通用小工具

use sha1::{Digest, Sha1};
use std::path::Path;

use crate::error::CoreResult;

/// 字节数组转小写十六进制字符串（避免引入 hex 依赖）
pub fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// 计算文件 SHA-1（流式读取，64KB 分块，零全量载入内存）
pub async fn sha1_file(path: &Path) -> CoreResult<String> {
    use tokio::io::AsyncReadExt;

    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha1::new();
    let mut buf = vec![0u8; 64 * 1024];

    loop {
        let read = file.read(&mut buf).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }

    Ok(to_hex(&hasher.finalize()))
}

/// 校验文件 SHA-1 是否匹配
pub async fn verify_sha1(path: &Path, expected: &str) -> bool {
    match sha1_file(path).await {
        Ok(actual) => actual.eq_ignore_ascii_case(expected),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_encoding_is_lowercase_and_padded() {
        assert_eq!(to_hex(&[0x00, 0x0f, 0xff, 0xa5]), "000fffa5");
        assert_eq!(to_hex(&[]), "");
    }

    #[tokio::test]
    async fn sha1_matches_known_vector() {
        // "abc" 的标准 SHA-1
        let dir = std::env::temp_dir().join("mcl-util-test");
        tokio::fs::create_dir_all(&dir).await.expect("建目录");
        let path = dir.join("abc.txt");
        tokio::fs::write(&path, b"abc").await.expect("写文件");

        let digest = sha1_file(&path).await.expect("计算 SHA-1");
        assert_eq!(digest, "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert!(
            verify_sha1(&path, &digest.to_uppercase()).await,
            "大小写不敏感"
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
