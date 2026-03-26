use clap::Parser;

/// 高性能USRBIO文件拷贝工具：从本地/NFS拷贝到3FS
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// 源文件或目录路径
    #[arg(short, long)]
    pub source: Option<std::path::PathBuf>,

    /// 目标路径（3FS）
    #[arg(short, long)]
    pub target: Option<std::path::PathBuf>,

    /// 3FS挂载点
    #[arg(short = 'm', long)]
    pub mount_point: Option<String>,

    /// 并发工作线程数
    #[arg(short = 'j', long, default_value = "4")]
    pub workers: usize,

    /// 块大小（字节），默认1MB
    #[arg(long, default_value = "1048576")]
    pub block_size: usize,

    /// Pipeline深度（每个线程的并发I/O数）
    #[arg(long, default_value = "2")]
    pub pipeline_depth: usize,

    /// 最大Pipeline深度（动态调整上限）
    #[arg(long, default_value = "8")]
    pub max_pipeline_depth: usize,

    /// 共享内存使用比例（占总共享内存的百分比）
    /// 例如：0.95 表示使用总共享内存的95%
    #[arg(long, default_value = "0.95")]
    pub shm_usage_ratio: f64,

    /// 是否启用断点续传（文件数>100时自动启用）
    #[arg(long, default_value = "true")]
    pub resume: bool,

    /// 仅显示共享内存状态，不执行拷贝
    #[arg(long)]
    pub shm_status: bool,

    /// 是否递归拷贝目录
    #[arg(short = 'r', long)]
    pub recursive: bool,

    /// 是否显示进度
    #[arg(long, default_value = "true")]
    pub progress: bool,

    /// 是否保留文件属性
    #[arg(long, default_value = "true")]
    pub preserve_attrs: bool,

    /// 是否启用debug模式，打印详细的堆栈跟踪信息
    #[arg(short = 'd', long)]
    pub debug: bool,
}
