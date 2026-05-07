use tracing::{info, warn};

const TARGET_RLIMIT: u64 = 65536;
const MIN_ACCEPTABLE: u64 = 4096;

pub fn raise_fd_limits() -> bool {
    let (soft, hard) = current_limits();
    if soft >= MIN_ACCEPTABLE {
        info!("FD limits: soft={}, hard={} — sufficient.", soft, hard);
        return true;
    }

    let desired = TARGET_RLIMIT.min(hard);
    if desired < MIN_ACCEPTABLE {
        warn!(
            "FD limits: soft={}, hard={} — hard limit too low (need >= {}). \
             Set 'ulimit -n 65536' or equivalent before starting Bittice.",
            soft, hard, MIN_ACCEPTABLE
        );
        return false;
    }

    match set_limits(desired, desired) {
        Ok(()) => {
            let (new_soft, new_hard) = current_limits();
            info!("FD limits raised: soft={}, hard={}", new_soft, new_hard);
            true
        }
        Err(e) => {
            warn!("FD limits: failed to raise ({}). Current soft={}, hard={}", e, soft, hard);
            false
        }
    }
}

fn current_limits() -> (u64, u64) {
    #[cfg(unix)]
    {
        let mut soft: libc::rlim_t = 0;
        let mut hard: libc::rlim_t = 0;
        unsafe {
            let mut rl: libc::rlimit = std::mem::zeroed();
            if libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) == 0 {
                soft = rl.rlim_cur;
                hard = rl.rlim_max;
            }
        }
        (soft as u64, hard as u64)
    }
    #[cfg(not(unix))]
    {
        (1024, 1024)
    }
}

fn set_limits(soft: u64, hard: u64) -> Result<(), String> {
    #[cfg(unix)]
    {
        let mut rl: libc::rlimit = unsafe { std::mem::zeroed() };
        rl.rlim_cur = soft as libc::rlim_t;
        rl.rlim_max = hard as libc::rlim_t;
        unsafe {
            if libc::setrlimit(libc::RLIMIT_NOFILE, &rl) != 0 {
                return Err("setrlimit failed".to_string());
            }
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (soft, hard);
        Err("not supported on this platform".to_string())
    }
}