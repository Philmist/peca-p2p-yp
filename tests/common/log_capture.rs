//! テスト全体で共有する tracing 出力キャプチャ(プロセス単一のグローバルサブスクライバ)。
//!
//! cucumber は 1 プロセスで全フィーチャを実行するため、tracing のグローバルサブスクライバは
//! 一度しか設定できない。秘密鍵非漏洩検査(keystore — FR-011)と PEX 良性破棄の debug ログ
//! 観測(security — feature 005 / US1 AC3 / SC-003)の双方が同じバッファを共有できるよう、
//! **DEBUG レベル**で一元化する。DEBUG 化は捕捉行を増やすのみで、秘密非漏洩の検査はより
//! 厳しくなる方向であり既存アサーションを壊さない。

use std::sync::{Arc, Mutex, Once, OnceLock};

/// プロセス全体で共有する tracing 出力バッファ。
fn log_buffer() -> &'static Arc<Mutex<Vec<u8>>> {
    static BUF: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
    BUF.get_or_init(|| Arc::new(Mutex::new(Vec::new())))
}

/// バッファへ書き込む `io::Write`(tracing の `MakeWriter` として使う)。
struct BufWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for BufWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Ok(mut b) = self.0.lock() {
            b.extend_from_slice(buf);
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// キャプチャ用サブスクライバを一度だけ設定する(既に設定済みなら何もしない)。
///
/// **DEBUG レベル**で設定するため、`p2p` ターゲットの debug ログ(良性破棄の観測 —
/// feature 005)も捕捉できる。最初の呼び出しが勝つが、全呼び出し元が本関数を使うため
/// レベルは常に DEBUG に揃う。
pub fn init_capture() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let buf = Arc::clone(log_buffer());
        let _ = tracing_subscriber::fmt()
            .with_writer(move || BufWriter(Arc::clone(&buf)))
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .try_init();
    });
}

/// キャプチャした全 tracing 出力。
pub fn captured_logs() -> String {
    log_buffer()
        .lock()
        .map(|b| String::from_utf8_lossy(&b).to_string())
        .unwrap_or_default()
}
