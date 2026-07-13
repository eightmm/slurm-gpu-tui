# sgpu - SLURM GPU 운영 모니터

SLURM GPU 실시간 모니터링: 터미널 TUI, collector 데몬, compute node push
에이전트, 사용량/낭비 집계, Slack 알림.

![CI](https://github.com/eightmm/slurm-gpu-tui/actions/workflows/test.yml/badge.svg)
![Python](https://img.shields.io/badge/python-3.10+-blue)
![License](https://img.shields.io/badge/license-MIT-green)

[English README](README.md)

<p align="center"><img src="docs/tab-gpu.svg" alt="sgpu GPU tab" width="100%"></p>

**낭비 GPU 팝업 (`w`)** — idle / parked / rogue, 심한 순
<p><img src="docs/waste.svg" alt="waste popup" width="100%"></p>

**CPU 탭 (`2`)** — CPU 전용 노드 포함, 코어 할당, 유저별 코어
<p><img src="docs/tab-cpu.svg" alt="CPU tab" width="100%"></p>

**상세 열 (`d`)** — 온도, 전력, JobID, 잡 이름
<p><img src="docs/tab-gpu-details.svg" alt="details" width="100%"></p>

**유저별 GPU-hours (`3`)** — 할당 vs 실제 연산, 효율 %
<p><img src="docs/tab-usage.svg" alt="usage tab" width="100%"></p>


## 한눈에 보이는 것들

- 노드별 GPU 상태(사용률, VRAM, 온도, 전력)와 CPU/메모리
- 누가 어떤 GPU를 쓰는지 — SLURM Job과 매칭, 드라이버 probe 순서가
  `/dev/nvidiaN`과 달라도 정확
- 대기 큐(대기 이유 코드 + 예상 시작 시각)
- 유저별 GPU-hours, 효율, 낭비(idle/parked) 시간 + 일별 추이 —
  slurmdbd에서 백필하므로 collector가 멈춰도 수치 유지
- 잡 히스토리 + 월간 리포트 (`--jobs`, `--report`)
- 내 잡 취소(`x`), 노드 접기/펼치기, 유휴 필터, 실시간 검색
- 항시 실행 Collector 데몬으로 즉시 시작 (실행 시 SSH 대기 없음)
- Slack 알림: 노드 다운/복구, GPU 헬스(온도/ECC), 낭비/rogue GPU, 수집 중단 —
  일별 스레드, 영어 또는 한국어

## 동작 방식

`sgpu`는 `sinfo`/`squeue`(선택적으로 `sacct`)가 있고 GPU 노드로 passwordless
SSH가 되는 SLURM 로그인/마스터 노드에서 운영합니다.

```
[sgpu-agent @ each node]  ──3s──→  <AGENT_DIR>/<node>.json   (shared FS push)
                                          │
[sgpu-collector @ master] ──merge──→  /tmp/slurm-gpu-tui/data.json
                                          ↑
[sgpu TUI]                ──reads──┘   (instant, no SSH on launch)
```

- **Push 모드 (권장):** 각 GPU 노드의 상주 `sgpu-agent`가 통계를 공유 FS
  디렉토리에 기록하고, collector는 로컬로 읽음 — 핫패스에 SSH 없음. collector가
  에이전트를 자동 배포·수리(노드별 rate-limit)하므로 노드별 설치 불필요.
- **SSH-pull 폴백:** 살아있는 에이전트가 없는 노드는 SSH로 수집(ControlMaster
  풀, 비동기). 두 모드는 자유롭게 혼용.
- TUI는 병합 JSON을 읽으므로 클러스터 규모와 무관하게 즉시 시작. collector가
  없으면 직접 SSH 수집으로 폴백(첫 로딩 느림).
- collector는 `/tmp/slurm-gpu-tui/metrics.prom`(Prometheus textfile)도 기록 —
  node_exporter에 붙이고 포함된 대시보드 import. **→ [docs/GRAFANA.md](docs/GRAFANA.md)**
- 설치 후 또는 데이터 이상 시 첫 확인은 `sgpu doctor`.

---

## 설치

> **이미 설치된 서버라면? 그냥 `sgpu`.**

한 줄로 설치 또는 그 자리 업그레이드(최신으로 리셋 → venv 재구성 → collector
재시작; 노드 에이전트는 다음 사이클에 자동 재기동):

```bash
curl -fsSL https://raw.githubusercontent.com/eightmm/slurm-gpu-tui/main/bootstrap.sh | bash
```

> **root(또는 passwordless sudo)로 실행** 시 시스템 서비스 + 모든 유저용
> `/usr/local/bin/sgpu`. 일반 유저 설치는 본인 계정만 설정.

### 설치 위치 (`SGPU_INSTALL_DIR`)

기본값 `~/.sgpu/app` (root면 `/opt/sgpu`). 변경하려면 변수를 파이프의
**`bash` 쪽**에 붙일 것. **push 모드**를 쓰려면 설치 디렉토리와
`SLURM_GPU_TUI_AGENT_DIR`을 둘 다 노드가 같은 경로로 마운트하는 공유
파일시스템에 둘 것(아니면 자동으로 SSH-pull):

```bash
# 로컬 설치 (SSH-pull 모드)
curl -fsSL https://raw.githubusercontent.com/eightmm/slurm-gpu-tui/main/bootstrap.sh \
  | SGPU_INSTALL_DIR=/opt/sgpu bash

# 노드 push 모드 (설치 + 에이전트 경로를 공유 FS에, 예: NFS /home)
curl -fsSL https://raw.githubusercontent.com/eightmm/slurm-gpu-tui/main/bootstrap.sh \
  | SGPU_INSTALL_DIR=/home/shared/sgpu SLURM_GPU_TUI_AGENT_DIR=/home/shared/sgpu-nodes bash
```

설치 스크립트가 이 경로를 systemd 유닛에 구워 넣고 서비스 모드를 자동 선택:

| 환경 | 서비스 |
|------|--------|
| root / sudo | systemd 시스템 서비스 + 모든 유저용 `/usr/local/bin/sgpu` |
| sudo 없음, systemd `--user` | 유저 서비스(로그인 시 자동 시작) + PATH 추가 |
| sudo 없음, systemd 없음 | 백그라운드 프로세스 + PATH 추가 |

안내가 나오면 `source ~/.bashrc`(또는 새 터미널)로 PATH 반영. sudo가 있으면
심링크 자동 생성. 설치 디렉토리를 옮기면 위 명령 재실행.

> push 모드는 root→노드 passwordless SSH와, `root_squash` NFS에서는 쓰기 가능한
> 에이전트 경로도 필요. **→ [docs/PUSH.md](docs/PUSH.md)**

---

## 사용법

```bash
sgpu        # GPU 모니터 실행
```

### 키보드 단축키

| 키 | 동작 |
|----|------|
| `1` `2` `3` | 탭: GPU / CPU / Usage (CPU 탭은 CPU 전용 노드 + 유저별 코어 TOP) |
| `r` | 즉시 새로고침 |
| `s` | 정렬 순환: 노드명 → 사용률 → 유저 → 빈 GPU |
| `u` | 유저 필터(내가 첫 항목); 다시 누르면 해제 |
| `i` | 빈 GPU 필터 |
| `d` | 상세 컬럼 토글(온도 / 전력 / JobID / JobName) |
| `Space` | 노드 접기 / 펼치기 |
| `/` | 노드명 또는 유저명 검색(`Esc` 초기화) |
| `j` / `k` | 커서 아래 / 위 |
| `Enter` | Job / 노드 상세(`scontrol show`) |
| `w` | 낭비 GPU 팝업(idle / parked, 심한 순) |
| `x` | 커서 위치 잡 취소(본인 잡, 먼저 확인) |
| `e` | 스냅샷 JSON 내보내기 |
| `?` / `q` | 도움말 / 종료 |

TUI는 열려 있는 동안 토스트도 표시: 내 잡 시작/종료, 노드 다운/복구.
(TUI 없이 알림 받으려면 Slack 알림 참고.)

### 원샷 CLI

```bash
sgpu --once          # 텍스트 스냅샷
sgpu --json          # JSON 스냅샷 (sgpu --json | jq ...)
sgpu --waste [-v]    # 유휴/parked/rogue GPU; 있으면 exit 1 (-v는 Command/WorkDir 추가)
sgpu doctor          # 자가진단: 데이터, 에이전트, slurm, sacct, webhook, 공유
sgpu --usage [일수]  # 유저별 GPU-hours + 효율 + 낭비 (기본 7일)
sgpu --usage 7 --daily                 # + 일별 클러스터 추이 막대
sgpu --jobs [일수] [--user U]          # 잡 히스토리: 결과, GPU-hours, 대기 시간
sgpu --report [YYYY-MM]                # 월간 리포트(마크다운)
sgpu --wait-free 2 --partition heavy   # 빈 GPU 2개 생길 때까지 대기 후 exit 0
chkgpu               # 원샷 유저×노드 GPU/CPU 매트릭스 + next-free 예상시각
```

`--waste`를 cron+메일에 걸면 설정 없이 GPU 사재기 다이제스트;
`--wait-free`로 자리 나는 순간 잡 제출 스크립트 작성 가능.

### 화면 구성

```
▼ node01   ● idle   gpu_short   32/64   ████░░░░ 128/256G
               0   A100    ████████░  85%   █████░░  40/80G   72C   280W   eightmm  12345   2:30h
               1   A100    ░░░░░░░░░   0%   ░░░░░░░   0/80G   35C    45W
```

- **노드 헤더**(초록): 노드명, 상태, 파티션, CPU 할당/전체, RAM 바 + GPU당
  글리프 스트립(`█` 사용 중 · `▅` parked · `▂` 예약-유휴 · `▁` 빈 GPU ·
  `!` rogue)과 busy/free/waste 집계. `Space`로 접으면 노드당 한 줄.
- **`user !gres` / `user !slurm`(빨강)**: 해당 GPU에 SLURM 할당 없이 프로세스
  실행 중. `!gres` = 그 유저 잡이 `--gres` 빼고 제출됨(`w` 팝업에 잡ID 연결),
  `!slurm` = SLURM 밖 생 프로세스. 둘 다 ROGUE 칩 + `--waste` 최상단. 시스템
  데몬 제외(`SLURM_GPU_TUI_ROGUE_IGNORE`).
- **`user idle 3.2h`**: 할당됐지만 프로세스 없음 + 유휴 지속(1h 넘으면 노란
  강조 — 회수 후보).
- **`parked` 배지**: VRAM만 잡고 연산 ~0%. **FREE 칩**: 빈 GPU 수 + 위치.
- **상태 기호**: `●` idle · `◐` mixed · `○` alloc · `✖` drain.
- **오류 노드**: 원인 라벨(`~timeout`, `~unreachable`, `~smi_err`).

---

## Slack 알림

Collector가 클러스터 알림을 Slack으로 전송 — 노드 다운/복구, GPU 헬스(온도/ECC),
낭비/rogue GPU, 수집 중단 — 일별 스레드, 영어 또는 한국어. 설정은
`~/.sgpu/webhook.json`(핫 리로드); 설치 스크립트가 세팅하고 `sgpu doctor`가
현재 모드 표시.

**→ 전체 설정, 봇/스레드 모드, 모든 설정 키: [docs/ALERTS.md](docs/ALERTS.md)**

---

## Collector 데몬 관리

```bash
# 시스템 서비스 (root/sudo)
systemctl status|restart|stop sgpu-collector
journalctl -u sgpu-collector -f

# 유저 서비스 (sudo 없음)
systemctl --user status|restart|stop sgpu-collector
journalctl --user -u sgpu-collector -f

# 백그라운드 프로세스 (systemd 없음)
pgrep -a -f sgpu-collector      # 확인
tail -f /tmp/sgpu-collector.log # 로그
pkill -f sgpu-collector         # 중지
```

개발 체크아웃을 별도 prod venv로 배포하려면 `deploy.sh` 참고.

## 제거

한 줄 — collector·노드 에이전트 중지, 서비스·심링크·데이터·설치 디렉토리 삭제:

```bash
curl -fsSL https://raw.githubusercontent.com/eightmm/slurm-gpu-tui/main/uninstall.sh | bash
```

노드 에이전트만:
```bash
for n in $(sinfo -N -h -o %N | sort -u); do ssh "$n" 'pkill -f "bin/[s]gpu-agent"' 2>/dev/null; done
rm -rf "$SLURM_GPU_TUI_AGENT_DIR"   # 기본 ~/.sgpu/nodes
```

---

## 트러블슈팅

| 증상 | 확인 |
|------|------|
| `sgpu` 못 찾음 | `export PATH="$HOME/.sgpu/app/bin:$PATH"` (또는 설치 경로) |
| 매번 느린 시작 | collector 미실행 — `systemctl status sgpu-collector` |
| 노드 `~timeout` / `~unreachable` | 마스터→노드 SSH 실패 — `ssh -v <node>` |
| 노드 `~smi_err` / `~no_smi` | `ssh <node> nvidia-smi` |
| collector 크래시 | `journalctl -u sgpu-collector -n 50 --no-pager` (또는 `/tmp/sgpu-collector.log`) |
| 그 외 | `sgpu doctor` |

깨끗한 재설치는 한 줄 설치 명령 재실행.

---

## 환경 변수

| 변수 | 기본값 | 설명 |
|------|--------|------|
| `SLURM_GPU_TUI_REFRESH_SEC` | `3` | TUI 갱신 주기 |
| `SLURM_GPU_TUI_COLLECTOR_SEC` | `3` | Collector 수집 주기 |
| `SLURM_GPU_TUI_NODE_TIMEOUT_SEC` | `30` | 노드 SSH 타임아웃 |
| `SLURM_GPU_TUI_MAX_WORKERS` | `8` | 병렬 SSH 워커 (폴백 모드) |
| `SLURM_GPU_TUI_DATA_DIR` | `/tmp/slurm-gpu-tui` | 데몬 JSON 출력 경로 |
| `SLURM_GPU_TUI_STATE_DIR` | `~/.sgpu/state` | 영속 상태(usage, 낭비 나이, 인벤토리) |
| `SLURM_GPU_TUI_AGENT_DIR` | `~/.sgpu/nodes` | push 에이전트 데이터 경로(push 모드는 공유 FS) |
| `SLURM_GPU_TUI_AGENT_SEC` | `3` | 노드 에이전트 수집 주기 |
| `SLURM_GPU_TUI_AGENT_MAX_AGE_SEC` | `45` | 에이전트 데이터 신선도 한계 |
| `SLURM_GPU_TUI_AGENT_REPAIR_SEC` | `180` | 노드당 에이전트 수리 최소 간격 |
| `SLURM_GPU_TUI_AGENT_DISABLE` | (없음) | push 에이전트 완전 비활성화 |
| `SLURM_GPU_TUI_WASTE_MIN_SEC` | `600` | 낭비 뷰 / `--waste` 임계값 |
| `SLURM_GPU_TUI_AUTO_COLLAPSE_NODES` | `12` | GPU 노드가 이 수 이상이면 접힌 상태로 시작 |
| `SLURM_GPU_TUI_USAGE_KEEP_DAYS` | `30` | GPU-hour 히스토리 보존 |
| `SLURM_GPU_TUI_SACCT_SEC` | `3600` | slurmdbd 백필 주기; `0`이면 비활성화(3회 실패 후 자동 비활성화) |
| `SLURM_GPU_TUI_WEBHOOK_URL` | (없음) | Slack webhook URL 단축(전체 설정은 `~/.sgpu/webhook.json`) |
| `SLURM_GPU_TUI_SLACK_BOT_TOKEN` | (없음) | 일별 스레드 모드용 Slack 봇 토큰 |
| `SLURM_GPU_TUI_WEBHOOK_DEBOUNCE_SEC` | `1800` | 같은 이벤트 키 반복 알림 최소 간격 |
| `SLURM_GPU_TUI_WEBHOOK_NAG_SEC` | `21600` | 지속 상태(낭비/rogue/온도/ECC) 재알림 간격 |
| `SLURM_GPU_TUI_ROGUE_IGNORE` | `root,gdm,xdm` | rogue로 안 잡을 유저 |
| `SLURM_GPU_TUI_SHARE_SCRIPTS` | (없음) | 전체 잡 batch script를 모든 유저에게 Enter 팝업에 공개. **스크립트 내용(비밀키 포함)이 전원 공개** — 설치 시 질문(`SGPU_SHARE_SCRIPTS=0/1`이면 생략) |

설치 시에만: `SGPU_INSTALL_DIR` (repo + venv 위치).

---

## 요구 사항

- Python 3.10+
- SLURM (마스터 노드에서 `sinfo` / `squeue`)
- 마스터 → 컴퓨트 노드 passwordless SSH
- GPU 노드에 `nvidia-smi`
