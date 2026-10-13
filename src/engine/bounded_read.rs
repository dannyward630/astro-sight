use std::io::Read;
use std::path::Path;

/// UTF-8 テキストファイルを指定上限まで読み込む。
///
/// metadata 確認後にファイルが拡大する TOCTOU も、上限 + 1 byte だけ読むことで検出する。
pub(crate) fn read_utf8_file_limited(path: &Path, max_bytes: u64) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > max_bytes {
        return None;
    }
    read_utf8_limited(file, max_bytes)
}

fn read_utf8_limited(reader: impl Read, max_bytes: u64) -> Option<String> {
    let mut bytes = Vec::new();
    reader
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > max_bytes {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// ファイルをバイト列として指定上限まで読み込む。通常ファイルでない・読めない・上限を
/// 超える場合は `None`。
///
/// metadata 確認後にファイルが拡大する TOCTOU も、上限 + 1 byte だけ読むことで検出する。
pub(crate) fn read_file_limited(path: &Path, max_bytes: u64) -> Option<Vec<u8>> {
    let file = std::fs::File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > max_bytes {
        return None;
    }
    read_bytes_limited(file, max_bytes)
}

fn read_bytes_limited(reader: impl Read, max_bytes: u64) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > max_bytes {
        return None;
    }
    Some(bytes)
}

/// 入力の上限 (`MAX_INPUT_SIZE`、`parser::read_file` と同じ 100MB) までファイルを読む。
/// 解析や証明のためにプロジェクト内のファイルを読む箇所で、上限なしの `std::fs::read` の
/// 代わりに使う (1 ファイルでメモリが際限なく増えないようにする)。超過は読めない場合と
/// 同じく `None` なので、呼び出し側は既存の失敗時の扱い (証明しない側) に倒れる。
pub(crate) fn read_input_file(path: &Path) -> Option<Vec<u8>> {
    read_file_limited(path, crate::commands::MAX_INPUT_SIZE as u64)
}

/// `read_input_file` の UTF-8 テキスト版 (Cargo.toml などの設定ファイル用)。
pub(crate) fn read_input_text_file(path: &Path) -> Option<String> {
    read_utf8_file_limited(path, crate::commands::MAX_INPUT_SIZE as u64)
}

#[cfg(test)]
mod tests {
    use super::read_utf8_limited;

    #[test]
    fn accepts_exact_limit() {
        let content = std::io::Cursor::new(b"test".to_vec());
        assert_eq!(read_utf8_limited(content, 4), Some("test".to_string()));
    }

    #[test]
    fn rejects_one_byte_over_limit() {
        let content = std::io::Cursor::new(b"tests".to_vec());
        assert!(read_utf8_limited(content, 4).is_none());
    }

    #[test]
    fn rejects_invalid_utf8() {
        let content = std::io::Cursor::new(vec![0xff]);
        assert!(read_utf8_limited(content, 1).is_none());
    }

    /// バイト列版も上限ちょうどは読み、1 byte でも超えたら読まない。ディスクを消費しない
    /// 疎ファイルで metadata 側の判定を、`take` 側の判定は reader で確かめる。
    #[test]
    fn byte_reader_enforces_limit_and_regular_files() {
        use super::{read_bytes_limited, read_file_limited};
        assert_eq!(
            read_bytes_limited(std::io::Cursor::new(b"test".to_vec()), 4),
            Some(b"test".to_vec())
        );
        assert!(read_bytes_limited(std::io::Cursor::new(b"tests".to_vec()), 4).is_none());

        let dir = tempfile::tempdir().expect("tempdir");
        let exact = dir.path().join("exact.rs");
        std::fs::write(&exact, b"fn a() {}\n").expect("write");
        assert_eq!(
            read_file_limited(&exact, 10).as_deref(),
            Some(&b"fn a() {}\n"[..])
        );
        let sparse = dir.path().join("huge.rs");
        std::fs::File::create(&sparse)
            .and_then(|file| file.set_len(11))
            .expect("sparse file");
        assert!(
            read_file_limited(&sparse, 10).is_none(),
            "上限超過は読まない"
        );
        assert!(
            read_file_limited(dir.path(), 10).is_none(),
            "ディレクトリは読まない"
        );
        assert!(read_file_limited(&dir.path().join("missing.rs"), 10).is_none());
    }
}
