//! 统一的 ssh 调用参数与错误摘要。

use std::io::{self, ErrorKind, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

pub struct Timeouts {
    pub connect: u32,
    pub alive_interval: u32,
    pub alive_count: u32,
}

/// 探测：快速失败
pub const PROBE: Timeouts = Timeouts {
    connect: 5,
    alive_interval: 10,
    alive_count: 2,
};

/// 传输：大文件可能要传几分钟
pub const TRANSFER: Timeouts = Timeouts {
    connect: 8,
    alive_interval: 20,
    alive_count: 3,
};

/// 所有远端命令都经由这里构造 ssh：
/// - `BatchMode=yes`：定时任务里不能弹密码 / 主机指纹确认；
/// - `-T` + `RemoteCommand=none`：ssh config 里常给交互登录配 `RemoteCommand`
///   （如 `cd ~/Workspace; exec $SHELL -l`），与命令行命令冲突会直接报
///   "Cannot execute command-line and remote command."。
pub fn command(host: &str, t: &Timeouts) -> Command {
    let mut cmd = Command::new("ssh");
    cmd.args(["-T", "-o", "BatchMode=yes", "-o", "RemoteCommand=none"])
        .arg("-o")
        .arg(format!("ConnectTimeout={}", t.connect))
        .arg("-o")
        .arg(format!("ServerAliveInterval={}", t.alive_interval))
        .arg("-o")
        .arg(format!("ServerAliveCountMax={}", t.alive_count))
        .arg(host);
    cmd
}

/// 在远端用 `sh -s` 执行脚本：脚本经 stdin 传过去，不经过登录 shell 解析——
/// 登录 shell 是 fish / csh 时，把 POSIX 脚本直接当命令行传过去会语法错误。
/// stderr 总是管道，调用方负责读取（或用 `wait_with_output`）。
pub fn spawn_script(host: &str, t: &Timeouts, script: &str, stdout: Stdio) -> io::Result<Child> {
    let mut child = command(host, t)
        .args(["sh", "-s"])
        .stdin(Stdio::piped())
        .stdout(stdout)
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        // 连不上时 ssh 可能已经退出，写入会报 BrokenPipe；真正的原因看退出码和 stderr。
        // 脚本远小于管道缓冲，写入不会阻塞；drop 即关闭 stdin，远端 sh 读到 EOF 后结束。
        let _ = stdin.write_all(script.as_bytes());
    }
    Ok(child)
}

/// 子进程的总时长上限。连接还活着、远端命令却卡死（如挂死的网络盘）时 ServerAlive 检测不到，
/// 没有上限就会一直占着运行锁，后面的同步全被跳过。超时后 kill 该进程；正常结束时调用
/// [`Watchdog::disarm`]。
pub struct Watchdog {
    done: mpsc::Sender<()>,
    fired: Arc<AtomicBool>,
    thread: thread::JoinHandle<()>,
}

impl Watchdog {
    pub fn arm(pid: u32, timeout: Duration) -> Watchdog {
        let (done, rx) = mpsc::channel::<()>();
        let fired = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&fired);
        let thread = thread::spawn(move || {
            if let Err(mpsc::RecvTimeoutError::Timeout) = rx.recv_timeout(timeout) {
                flag.store(true, Ordering::SeqCst);
                let _ = Command::new("kill").arg(pid.to_string()).status();
            }
        });
        Watchdog {
            done,
            fired,
            thread,
        }
    }

    /// 解除并返回是否已经因超时出过手。调用方应同时看子进程是否真被信号终止：
    /// 计时恰好在进程自己退出之后、解除之前到点时，这里也会是 true。
    pub fn disarm(self) -> bool {
        let _ = self.done.send(());
        let _ = self.thread.join();
        self.fired.load(Ordering::SeqCst)
    }
}

/// 读完整个流（对端不会因为管道写满而卡住），只保留最后 `cap` 字节——错误原因在末尾。
pub fn read_tail(mut r: impl Read, cap: usize) -> String {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match r.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > cap * 2 {
                    buf.drain(..buf.len() - cap);
                }
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    if buf.len() > cap {
        buf.drain(..buf.len() - cap);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// 子进程 stderr 的最后一行非空内容：真正的错误通常在最后，前面可能是 banner / 警告。
pub fn last_line(stderr: &str) -> Option<String> {
    stderr
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .map(|l| l.chars().take(200).collect())
}

/// ssh 失败的可读描述：优先 stderr 里的原因，附上退出码。
pub fn describe_exit(code: Option<i32>, stderr: &str) -> String {
    let status = match code {
        Some(c) => format!("ssh 退出码 {c}"),
        None => "ssh 被信号终止".to_string(),
    };
    match last_line(stderr) {
        Some(reason) => format!("{reason}（{status}）"),
        None => status,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn command_overrides_remote_command_and_tty() {
        let a = args(&command("macbookpro", &PROBE));
        assert!(a.contains(&"-T".to_string()));
        assert!(a.contains(&"RemoteCommand=none".to_string()));
        assert!(a.contains(&"BatchMode=yes".to_string()));
        assert!(a.contains(&"ConnectTimeout=5".to_string()));
        assert_eq!(
            a.last().map(String::as_str),
            Some("macbookpro"),
            "主机名放最后，后面才接远端命令"
        );
    }

    #[test]
    fn last_line_skips_warnings_and_blanks() {
        let err = "Warning: Permanently added 'x' (ED25519) to the list of known hosts.\n\
                   Timeout, server 1.2.3.4 not responding.\n\n";
        assert_eq!(
            last_line(err).as_deref(),
            Some("Timeout, server 1.2.3.4 not responding.")
        );
        assert_eq!(last_line(" \n\n"), None);
    }

    #[test]
    fn watchdog_kills_a_stuck_process() {
        let mut stuck = Command::new("sleep").arg("30").spawn().unwrap();
        let started = std::time::Instant::now();
        let dog = Watchdog::arm(stuck.id(), Duration::from_millis(200));
        let status = stuck.wait().unwrap();
        assert!(dog.disarm(), "超时应当出手");
        assert!(status.code().is_none(), "应被信号终止");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn watchdog_stays_quiet_when_the_process_finishes() {
        let mut quick = Command::new("true").spawn().unwrap();
        let dog = Watchdog::arm(quick.id(), Duration::from_secs(60));
        assert!(quick.wait().unwrap().success());
        let started = std::time::Instant::now();
        assert!(!dog.disarm());
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "解除不能等到超时"
        );
    }

    #[test]
    fn read_tail_keeps_the_end_of_a_flood() {
        let mut flood = "noise\n".repeat(50_000);
        flood.push_str("Connection reset by peer\n");
        let tail = read_tail(std::io::Cursor::new(flood), 1024);
        assert!(tail.len() <= 1024);
        assert_eq!(
            last_line(&tail).as_deref(),
            Some("Connection reset by peer")
        );
    }

    #[test]
    fn describe_exit_prefers_stderr_reason() {
        assert_eq!(describe_exit(Some(255), ""), "ssh 退出码 255");
        assert_eq!(
            describe_exit(Some(255), "client_loop: send disconnect: Broken pipe\n"),
            "client_loop: send disconnect: Broken pipe（ssh 退出码 255）"
        );
        assert_eq!(describe_exit(None, ""), "ssh 被信号终止");
    }
}
