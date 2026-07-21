<div align="center">

# sgpu

**SLURM GPU 실시간 운영 모니터**
터미널 TUI · collector 데몬 · push 에이전트 · 사용량/낭비 집계 · Slack 알림

![CI](https://github.com/eightmm/slurm-gpu-tui/actions/workflows/test.yml/badge.svg)
![Python](https://img.shields.io/badge/python-3.10+-blue)
![License](https://img.shields.io/badge/license-MIT-green)

[English README](README.md)

</div>

<p align="center"><img src="docs/tab-gpu.svg" alt="sgpu GPU tab" width="100%"></p>

<table>
<tr>
<td width="50%" align="center"><img src="docs/waste.svg" alt="waste popup"><br>
<sub><b>낭비 GPU 팝업 (w)</b> — idle / parked / rogue, 심한 순</sub></td>
<td width="50%" align="center"><img src="docs/tab-cpu.svg" alt="CPU tab"><br>
<sub><b>CPU 탭 (2)</b> — CPU 전용 노드 포함, 유저별 코어</sub></td>
</tr>
<tr>
<td width="50%" align="center"><img src="docs/tab-usage.svg" alt="usage tab"><br>
<sub><b>유저별 GPU-hours (3)</b> — 할당 vs 실제 연산, 효율 %</sub></td>
<td width="50%" align="center"><img src="docs/tab-gpu-details.svg" alt="detail columns"><br>
<sub><b>상세 열 (d)</b> — 온도, 전력, JobID, 잡 이름</sub></td>
</tr>
</table>

## 기능

- 노드별 GPU 상태(사용률, VRAM, 온도, 전력)와 CPU/RAM — 드라이버 probe
  순서가 `/dev/nvidiaN`과 달라도 GPU를 SLURM Job에 정확히 매칭
- idle / parked / rogue GPU 탐지, 대기 큐(이유 코드 + 예상 시작 시각)
- 유저별 GPU-hours, 효율, 낭비 시간 — slurmdbd에서 백필
- Slack 알림(노드 다운/복구, GPU 헬스, 낭비/rogue)을 일별 스레드로 묶고,
  문제가 없는 날에도 부모 메시지 하나를 게시
- Prometheus 메트릭 + 포함된 Grafana 대시보드 — **[docs/GRAFANA.md](docs/GRAFANA.md)**

## 동작 방식

SLURM 로그인/마스터 노드에서 운영(Python 3.10+, `sinfo`/`squeue`, 선택적으로
`sacct`). 컴퓨트 노드로 passwordless SSH 필요, GPU 노드에는 `nvidia-smi`.

```
[sgpu-agent @ each node] ──3/20s─→ <AGENT_DIR>/<node>.json   (shared FS push)
                                          │
[sgpu-collector @ master] ──merge──→  /tmp/slurm-gpu-tui/data.json
                                          ↑
[sgpu TUI]                ──reads──┘   (instant, no SSH on launch)
```

- **Push 모드 (권장):** GPU agent는 3초마다 `nvidia-smi` 데이터를, CPU 전용
  agent는 20초마다 `/proc/meminfo`를 push — 핫패스에 SSH 없음.
- **SSH-pull 폴백:** 살아있는 에이전트가 없는 노드는 SSH로 수집(ControlMaster
  풀, 비동기). 두 모드 자유 혼용; CPU payload가 stale이면 저빈도 SSH polling
  으로 폴백(`cpu-poll` 표시).
- TUI는 병합 JSON을 읽으므로 클러스터 규모와 무관하게 즉시 시작.
- 설치 후 또는 데이터 이상 시 첫 확인은 `sgpu doctor`.

## 설치

> **이미 설치된 서버라면? 그냥 `sgpu`.**

한 줄로 설치 또는 그 자리 업그레이드:

```bash
curl -fsSL https://raw.githubusercontent.com/eightmm/slurm-gpu-tui/main/bootstrap.sh | bash
```

**root/sudo**로 실행 시 시스템 서비스 + 모든 유저용 `/usr/local/bin/sgpu`;
일반 유저 설치는 본인 계정만 설정. root 설치는 추가로 GPU 노드에 NVIDIA
persistence mode를 활성화하고(`SGPU_ENABLE_PERSISTENCE=0`으로 생략), 공유 FS
환경이면 CPU 전용 노드에 push 에이전트를 배치한다(`SGPU_ENABLE_CPU_PUSH=0`
으로 생략).

**설치 위치** (`SGPU_INSTALL_DIR`): 유저 설치는 `~/.sgpu/app`; root는
`/home/shared`가 있으면 `/home/shared/sgpu`(공유 FS → push 모드 기본 동작),
없으면 `/opt/sgpu`(SSH-pull). 변경하려면 변수를 파이프의 **`bash` 쪽**에
붙일 것 — push 모드는 두 경로 모두 컴퓨트 노드가 같은 경로로 마운트하는
공유 FS에 있어야 한다:

```bash
curl -fsSL https://raw.githubusercontent.com/eightmm/slurm-gpu-tui/main/bootstrap.sh \
  | SGPU_INSTALL_DIR=/nfs/apps/sgpu SLURM_GPU_TUI_AGENT_DIR=/nfs/apps/sgpu-nodes bash
```

| 환경 | 서비스 |
|------|--------|
| root / sudo | 시스템 서비스 + 모든 유저용 `/usr/local/bin/sgpu` |
| sudo 없음, systemd `--user` | 유저 서비스(로그인 시 자동 시작) + PATH 추가 |
| sudo 없음, systemd 없음 | 백그라운드 프로세스 + PATH 추가 |

> push 모드 상세(root→노드 SSH, `root_squash` NFS): **[docs/PUSH.md](docs/PUSH.md)**

## 사용법

```bash
sgpu        # 모니터 실행
```

| 키 | 동작 |
|----|------|
| `1` `2` `3` | 탭: GPU / CPU / Usage |
| `r` / `s` | 새로고침 / 정렬 순환(노드 → 사용률 → 유저 → 빈 GPU) |
| `u` / `i` | 유저 필터(내가 첫 항목) / 빈 GPU 필터 |
| `p` / `m` | 파티션 필터 순환 / 내 잡만 보기 |
| `d` | 상세 컬럼(온도 / 전력 / JobID / JobName) |
| `Space` / `j` `k` | 노드 접기 / 커서 이동 |
| `/` | 노드명 또는 유저명 검색(`Esc` 초기화) |
| `Enter` | Job / 노드 상세(`scontrol show`) — `Tab`으로 Info / Script / StdOut / StdErr 전환 |
| `w` | 낭비 GPU 팝업 |
| `h` | 내 잡 히스토리(7일, sacct) — `Enter`: 상태, 종료 코드, 로그 |
| `n` | 커서 위치 잡 watch — 시작/종료 시 토스트 |
| `x` | 커서 위치 잡 취소(본인 잡, 먼저 확인) |
| `e` | 스냅샷 JSON 내보내기 |
| `?` / `q` | 도움말 / 종료 |

TUI는 열려 있는 동안 토스트도 표시: 내 잡 시작/종료, 노드 다운/복구.

### 원샷 CLI

```bash
sgpu --once          # 텍스트 스냅샷 (--json은 JSON)
sgpu --waste [-v]    # 유휴/parked/rogue GPU; 있으면 exit 1
sgpu doctor          # 자가진단: 데이터, 에이전트, slurm, sacct, Slack
sgpu --usage [일수] [--daily]          # 유저별 GPU-hours + 효율 + 낭비
sgpu --jobs [일수] [--user U]          # 잡 히스토리: 결과, GPU-hours, 대기
sgpu logs JOBID [-f] [-e]              # 잡 stdout 꼬리 보기 (-e: stderr, -f: 따라가기)
sgpu --report [YYYY-MM]                # 월간 리포트(마크다운)
sgpu --wait-free 2 --partition heavy   # 빈 GPU 2개 생길 때까지 대기
sgpu fit 2 [--vram 40] [--partition P] # 지금 GPU 2장 들어갈 노드 + sbatch 예시
sgpu me              # 내 잡 · 내 낭비 GPU · 최근 7일 (낭비 있으면 exit 1)
chkgpu               # 원샷 유저×노드 매트릭스 + next-free 예상시각
```

### 화면 구성

```
▼ node01   ● idle   gpu_short   32/64   ████░░░░ 128/256G
               0   A100    ████████░  85%   █████░░  40/80G   72C   280W   eightmm  12345   2:30h
               1   A100    ░░░░░░░░░   0%   ░░░░░░░   0/80G   35C    45W
```

- **노드 헤더**: 노드명, 상태(`●` idle · `◐` mixed · `○` alloc · `✖` drain),
  파티션, CPU 할당, RAM 바, GPU당 글리프 스트립
  (`█` 사용 중 · `▅` parked · `▂` 예약-유휴 · `▁` 빈 GPU · `!` rogue)
- **`user !gres` / `user !slurm`(빨강)**: rogue — 해당 GPU에 SLURM 할당 없이
  프로세스 실행 중
- **`user idle 3.2h`**: 할당됐지만 프로세스 없음, 1h 넘으면 노란 강조
- **오류 노드**: `~timeout`, `~unreachable`, `~smi_err`

## Slack 알림

설정은 `~/.sgpu/slack.json`(핫 리로드); 설치 스크립트가 세팅하고
`sgpu doctor`가 현재 모드 표시. 전체 설정: **[docs/ALERTS.md](docs/ALERTS.md)**

## 운영

```bash
systemctl status|restart sgpu-collector          # root 설치
systemctl --user status|restart sgpu-collector   # 유저 설치
journalctl -u sgpu-collector -f                  # 로그 (또는 /tmp/sgpu-collector.log)
```

| 증상 | 확인 |
|------|------|
| `sgpu` 못 찾음 | `export PATH="$HOME/.sgpu/app/bin:$PATH"` |
| 매번 느린 시작 | collector 미실행 |
| 노드 `~timeout` / `~unreachable` | 마스터→노드 SSH 실패 |
| 노드 `~smi_err` / `~no_smi` | `ssh <node> nvidia-smi` |
| 그 외 | `sgpu doctor` |

깨끗한 재설치는 한 줄 설치 명령 재실행. 개발 체크아웃을 별도 prod venv로
배포하려면 `deploy.sh` 참고. 제거(collector·에이전트 중지, 서비스·데이터·설치
디렉토리 삭제):

```bash
curl -fsSL https://raw.githubusercontent.com/eightmm/slurm-gpu-tui/main/uninstall.sh | bash
```

## 설정

<details>
<summary><b>환경 변수</b> (기본값으로 바로 동작)</summary>

| 변수 | 기본값 | 설명 |
|------|--------|------|
| `SLURM_GPU_TUI_REFRESH_SEC` | `3` | TUI 갱신 주기 |
| `SLURM_GPU_TUI_COLLECTOR_SEC` | `3` | Collector 수집 주기 |
| `SLURM_GPU_TUI_NODE_TIMEOUT_SEC` | `30` | 노드 SSH 타임아웃 |
| `SLURM_GPU_TUI_MAX_WORKERS` | `8` | 병렬 SSH 워커(폴백 모드) |
| `SLURM_GPU_TUI_DATA_DIR` | `/tmp/slurm-gpu-tui` | 데몬 JSON 출력 경로 |
| `SLURM_GPU_TUI_STATE_DIR` | `~/.sgpu/state` | 영속 상태(usage, 낭비, 인벤토리) |
| `SLURM_GPU_TUI_AGENT_DIR` | `~/.sgpu/nodes` | push 에이전트 데이터 경로(push 모드는 공유 FS) |
| `SLURM_GPU_TUI_AGENT_SEC` | `3` | GPU 에이전트 주기 |
| `SLURM_GPU_TUI_CPU_AGENT_SEC` | `20` | CPU 전용 에이전트 주기 |
| `SLURM_GPU_TUI_AGENT_MAX_AGE_SEC` | `45` | 에이전트 데이터 신선도 한계 |
| `SLURM_GPU_TUI_AGENT_REPAIR_SEC` | `180` | 노드당 에이전트 수리 최소 간격 |
| `SLURM_GPU_TUI_AGENT_DISABLE` | (없음) | push 에이전트 완전 비활성화 |
| `SLURM_GPU_TUI_WASTE_MIN_SEC` | `600` | 낭비 뷰 / `--waste` 임계값 |
| `SLURM_GPU_TUI_USAGE_KEEP_DAYS` | `30` | GPU-hour 히스토리 보존 |
| `SLURM_GPU_TUI_SACCT_SEC` | `3600` | slurmdbd 백필 주기; `0`이면 비활성화 |
| `SLURM_GPU_TUI_SLACK_BOT_TOKEN` | (없음) | Slack 봇 토큰(채널은 `~/.sgpu/slack.json`에 설정) |
| `SLURM_GPU_TUI_SLACK_DEBOUNCE_SEC` | `1800` | 반복 알림 최소 간격 |
| `SLURM_GPU_TUI_SLACK_NAG_SEC` | `21600` | 지속 상태 재알림 간격 |
| `SLURM_GPU_TUI_ROGUE_IGNORE` | `root,gdm,xdm` | rogue로 안 잡을 유저 |
| `SLURM_GPU_TUI_SHARE_SCRIPTS` | (없음) | 전체 잡 batch script를 모든 유저에게 공개 — **스크립트 내용(비밀키 포함) 전원 공개** |

설치 시에만: `SGPU_INSTALL_DIR`, `SGPU_ENABLE_PERSISTENCE`(`0`이면 GPU 노드
persistence 생략), `SGPU_ENABLE_CPU_PUSH`(`0`이면 CPU telemetry를 SSH polling
으로 유지), `SGPU_SHARE_SCRIPTS`.

</details>
