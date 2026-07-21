"""_master_host_lines: the collector's own-host stats for the master row."""
import re

from sgpu.collector import _master_host_lines


def _fake_proc(tmp_path):
    proc = tmp_path / "proc"
    proc.mkdir()
    (proc / "stat").write_text(
        "cpu  100 0 50 9000 20 0 5 0 0 0\n"
        "cpu0 50 0 25 4500 10 0 2 0 0 0\n"
        "cpu1 50 0 25 4600 10 0 3 0 0 0\n"
        "btime 1700000000\n"
    )
    (proc / "meminfo").write_text(
        "MemTotal:       16384000 kB\nMemFree:         1000000 kB\n"
        "MemAvailable:    8192000 kB\n"
    )
    (proc / "loadavg").write_text("1.25 1.00 0.75 2/300 12345\n")
    (proc / "mounts").write_text(
        f"/dev/sda1 / ext4 rw 0 0\n"
        f"tmpfs /run tmpfs rw 0 0\n"
        f"/dev/sdb1 {tmp_path} ext4 rw 0 0\n"   # statvfs-able mountpoint
    )
    netdev = proc / "net"
    netdev.mkdir()
    (netdev / "dev").write_text(
        "Inter-|   Receive                                                |  Transmit\n"
        " face |bytes    packets errs drop fifo frame compressed multicast|bytes ...\n"
        "    lo: 111 1 0 0 0 0 0 0 111 1 0 0 0 0 0 0\n"
        "  eth0: 5000 10 0 0 0 0 0 0 7000 12 0 0 0 0 0 0\n"
    )
    (proc / "diskstats").write_text(
        " 8 0 sda 100 0 2048 50 200 0 4096 80 0 0 0\n"
        " 8 1 sda1 100 0 2048 50 200 0 4096 80 0 0 0\n"
        " 7 0 loop0 1 0 8 0 0 0 0 0 0 0 0\n"
    )
    return proc


def _fake_sys(tmp_path):
    sysd = tmp_path / "sys"
    hw = sysd / "class" / "hwmon" / "hwmon0"
    hw.mkdir(parents=True)
    (hw / "name").write_text("coretemp\n")
    (hw / "temp1_input").write_text("43500\n")
    (hw / "temp2_input").write_text("41000\n")
    return sysd


def test_master_host_lines(tmp_path):
    lines = _master_host_lines(proc=str(_fake_proc(tmp_path)),
                               sys_dir=str(_fake_sys(tmp_path)))
    text = "\n".join(lines)
    assert 'sgpu_master_cpu_seconds_total{cpu="0",mode="idle"} 45.00' in text
    assert 'sgpu_master_cpu_seconds_total{cpu="1",mode="idle"} 46.00' in text
    assert "sgpu_master_boot_time_seconds 1700000000" in text
    assert "sgpu_master_memory_MemTotal_bytes 16777216000" in text
    assert "sgpu_master_memory_MemAvailable_bytes 8388608000" in text
    assert "sgpu_master_load1 1.25" in text
    assert 'sgpu_master_network_receive_bytes_total{device="eth0"} 5000' in text
    assert 'sgpu_master_network_transmit_bytes_total{device="eth0"} 7000' in text
    assert "lo" not in text.replace("load1", "").replace("loop0", "")
    assert 'sgpu_master_disk_read_bytes_total{device="sda"} 1048576' in text
    assert 'sgpu_master_disk_written_bytes_total{device="sda"} 2097152' in text
    assert "sda1" not in text and "loop0" not in text  # whole disks only
    assert 'sgpu_master_hwmon_temp_celsius{chip="platform_coretemp.0",sensor="temp1"} 43.5' in text
    assert "filesystem_size_bytes" in text  # tmp_path mount via statvfs


def test_master_host_lines_no_duplicate_series(tmp_path):
    lines = _master_host_lines(proc=str(_fake_proc(tmp_path)),
                               sys_dir=str(_fake_sys(tmp_path)))
    keys = [ln.rsplit(" ", 1)[0] for ln in lines]
    assert len(keys) == len(set(keys))  # node_exporter rejects duplicates


def test_master_host_lines_missing_proc_is_empty(tmp_path):
    assert _master_host_lines(proc=str(tmp_path / "nope"),
                              sys_dir=str(tmp_path / "nosys")) == []


def test_master_host_lines_real_host_parses():
    # smoke on the real /proc — values must be numeric prom lines
    for ln in _master_host_lines():
        assert re.match(r'^sgpu_master_[A-Za-z0-9_]+(\{[^}]*\})? [0-9.e+-]+$', ln), ln
