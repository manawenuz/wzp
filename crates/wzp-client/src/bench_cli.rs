//! WarzonePhone benchmark CLI.
//!
//! Usage: wzp-bench [--codec] [--fec] [--crypto] [--pipeline] [--all]
//!        wzp-bench --fec --loss 30   (test FEC with 30% loss)

use wzp_client::bench;

fn print_header(title: &str) {
    println!();
    println!("┌─────────────────────────────────────────────────────┐");
    println!("│ {:<51} │", title);
    println!("├─────────────────────────────────────────────────────┤");
}

fn print_row(label: &str, value: &str) {
    println!("│  {:<28} {:>20}  │", label, value);
}

fn print_footer() {
    println!("└─────────────────────────────────────────────────────┘");
}

fn run_codec() {
    print_header("Codec Roundtrip (Opus 24kbps)");
    let r = bench::bench_codec_roundtrip();
    print_row("Frames", &format!("{}", r.frames));
    print_row("Encode total", &format!("{:.2} ms", r.total_encode.as_secs_f64() * 1000.0));
    print_row("Decode total", &format!("{:.2} ms", r.total_decode.as_secs_f64() * 1000.0));
    print_row("Avg encode", &format!("{:.1} us", r.avg_encode_us));
    print_row("Avg decode", &format!("{:.1} us", r.avg_decode_us));
    print_row("Throughput", &format!("{:.0} frames/sec", r.frames_per_sec));
    print_row("Compression ratio", &format!("{:.1}x", r.compression_ratio));
    print_footer();
}

fn run_fec(loss_pct: f32) {
    print_header(&format!("FEC Recovery (loss={:.0}%)", loss_pct));
    let r = bench::bench_fec_recovery(loss_pct);
    print_row("Blocks attempted", &format!("{}", r.blocks_attempted));
    print_row("Blocks recovered", &format!("{}", r.blocks_recovered));
    print_row("Recovery rate", &format!("{:.1}%", r.recovery_rate_pct));
    print_row("Source bytes", &format!("{}", r.total_source_bytes));
    print_row("Repair (overhead) bytes", &format!("{}", r.overhead_bytes));
    print_row("Total time", &format!("{:.2} ms", r.total_time.as_secs_f64() * 1000.0));
    print_footer();
}

fn run_crypto() {
    print_header("Crypto (ChaCha20-Poly1305)");
    let r = bench::bench_encrypt_decrypt();
    print_row("Packets", &format!("{}", r.packets));
    print_row("Total time", &format!("{:.2} ms", r.total_time.as_secs_f64() * 1000.0));
    print_row("Throughput", &format!("{:.0} pkt/sec", r.packets_per_sec));
    print_row("Bandwidth", &format!("{:.2} MB/sec", r.megabytes_per_sec));
    print_row("Avg latency", &format!("{:.2} us", r.avg_latency_us));
    print_footer();
}

fn run_pipeline() {
    print_header("Full Pipeline (E2E)");
    let r = bench::bench_full_pipeline();
    print_row("Frames", &format!("{}", r.frames));
    print_row("Encode pipeline", &format!("{:.2} ms", r.total_encode_pipeline.as_secs_f64() * 1000.0));
    print_row("Decode pipeline", &format!("{:.2} ms", r.total_decode_pipeline.as_secs_f64() * 1000.0));
    print_row("Avg E2E latency", &format!("{:.1} us/frame", r.avg_e2e_latency_us));
    print_row("PCM in", &format!("{} bytes", r.pcm_bytes_in));
    print_row("Wire out", &format!("{} bytes", r.wire_bytes_out));
    print_row("Overhead ratio", &format!("{:.3}x", r.overhead_ratio));
    print_footer();
}

fn print_usage() {
    println!("Usage: wzp-bench [OPTIONS]");
    println!();
    println!("Options:");
    println!("  --codec       Run codec roundtrip benchmark");
    println!("  --fec         Run FEC recovery benchmark");
    println!("  --crypto      Run encryption benchmark");
    println!("  --pipeline    Run full pipeline benchmark");
    println!("  --all         Run all benchmarks (default)");
    println!("  --loss <N>    FEC loss percentage (default: 20)");
    println!("  --help        Show this help");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_usage();
        return;
    }

    let mut run_codec_flag = false;
    let mut run_fec_flag = false;
    let mut run_crypto_flag = false;
    let mut run_pipeline_flag = false;
    let mut loss_pct: f32 = 20.0;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--codec" => run_codec_flag = true,
            "--fec" => run_fec_flag = true,
            "--crypto" => run_crypto_flag = true,
            "--pipeline" => run_pipeline_flag = true,
            "--all" => {
                run_codec_flag = true;
                run_fec_flag = true;
                run_crypto_flag = true;
                run_pipeline_flag = true;
            }
            "--loss" => {
                i += 1;
                if i < args.len() {
                    loss_pct = args[i].parse().unwrap_or(20.0);
                }
            }
            other => {
                eprintln!("Unknown option: {}", other);
                print_usage();
                std::process::exit(1);
            }
        }
        i += 1;
    }

    // Default: run all if no specific flag given
    if !run_codec_flag && !run_fec_flag && !run_crypto_flag && !run_pipeline_flag {
        run_codec_flag = true;
        run_fec_flag = true;
        run_crypto_flag = true;
        run_pipeline_flag = true;
    }

    println!("=== WarzonePhone Protocol Benchmark ===");

    if run_codec_flag {
        run_codec();
    }
    if run_fec_flag {
        run_fec(loss_pct);
    }
    if run_crypto_flag {
        run_crypto();
    }
    if run_pipeline_flag {
        run_pipeline();
    }

    println!();
    println!("Done.");
}
