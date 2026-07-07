# sgpu - SLURM GPU Monitor

터미널에서 SLURM 클러스터의 GPU 사용 현황을 실시간으로 확인하는 TUI 도구입니다.

![Python](https://img.shields.io/badge/python-3.10+-blue)

[English README](README.md)

## 한눈에 보이는 것들

- 노드별 GPU 상태 (사용률, VRAM, 온도, 전력)
- 노드별 CPU 할당량 / 메모리 사용량
- 누가 어떤 GPU를 쓰고 있는지 (SLURM Job과 매칭)
- 대기 중인 Job 목록 및 대기 이유
- 사용자별 GPU 할당 요약
- 노드 접기/펼치기, 유휴 GPU 필터, 실시간 검색
- 항시 실행 Collector 데몬으로 즉시 시작 — 실행 시 SSH 대기 없음

---

## 설치

> **이미 설치된 서버라면?** 그냥 `sgpu`만 치면 됩니다.

### 한 줄 설치 / 업그레이드

```bash
curl -fsSL https://raw.githubusercontent.com/eightmm/slurm-gpu-tui/main/bootstrap.sh | bash
```

> **root(또는 passwordless sudo)로 실행 권장.** 시스템 서비스 + `/usr/local/bin/sgpu`가
> 설치돼 로그인 노드의 모든 유저가 바로 쓸 수 있습니다. 일반 유저로 설치하면
> 본인 계정용으로만 설정됩니다.

이미 설치돼 있어도 같은 명령을 다시 실행하면 **그 자리에서 업그레이드**됩니다:
최신 코드로 리셋 → venv 재구성 → collector 재시작, 노드의 push 에이전트도
다음 collector 사이클에 자동 교체됩니다.

설치 위치 기본값 `~/.sgpu/app` (root면 `/opt/sgpu` — `/root`는 다른 유저가 못 읽음).
변경하려면 변수를 파이프의 `bash` 쪽에 붙일 것 — push 에이전트를 쓰려면 계산
노드에서 venv를 실행할 수 있는 공유 파일시스템 경로로 (아니면 자동 SSH pull 모드):

```bash
curl -fsSL https://raw.githubusercontent.com/eightmm/slurm-gpu-tui/main/bootstrap.sh | SGPU_INSTALL_DIR=/공유경로/sgpu bash
```

설치 스크립트가 환경을 자동으로 감지해서 전부 처리합니다:

| 상황 | 자동 처리 내용 |
|------|--------------|
| **sudo 있음** | systemd 시스템 서비스 설치 + `/usr/local/bin/sgpu` 심볼릭 링크 생성 |
| **sudo 없음, systemd --user 지원** | systemd 유저 서비스 설치 (로그인 시 자동 시작) |
| **sudo 없음, systemd 없음** | 백그라운드 프로세스로 시작 + `~/.bashrc` 자동 추가 |

설치 후 PATH 반영 (sudo 없는 경우):

```bash
source ~/.bashrc   # 또는 터미널 새로 열기
sgpu
```

sudo가 있으면 `/usr/local/bin/sgpu` 심볼릭 링크가 자동으로 생성되므로 PATH 설정 불필요.

> **설치 디렉토리를 옮기면?** 위 설치 명령을 다시 실행하면 됩니다.

---

## 사용법

```bash
sgpu        # GPU 모니터 실행
```

### 키보드 단축키

| 키 | 동작 |
|----|------|
| `1` `2` `3` | 탭 전환: GPU / CPU / Usage |
| `r` | 즉시 새로고침 |
| `s` | 정렬 순환: 노드명 → 사용률 → 유저 → 빈 GPU |
| `u` | 유저 필터 — 목록에서 선택 (내가 첫 항목); 다시 누르면 해제 |
| `i` | 유휴 필터 토글 (빈 GPU 있는 노드만 표시) |
| `d` | 상세 컬럼 토글 (온도 / 전력 / JobID / JobName) |
| `Space` | 노드 접기 / 펼치기 (노드 헤더 행에 커서 위치 필요) |
| `/` | 노드명 또는 유저명으로 검색 — `Esc`로 초기화 |
| `j` / `k` | 커서 아래 / 위 이동 (vim 스타일) |
| `Enter` | Job / 노드 상세 팝업 (`scontrol show`) |
| `w` | 낭비 GPU 팝업 (idle / parked, 심한 순) |
| `g` | Usage 탭 열기 (유저별 GPU-hours) |
| `e` | 현재 상태를 JSON 파일로 내보내기 |
| `?` | 도움말 오버레이 |
| `q` | 종료 |

### 원샷 CLI 모드

```bash
sgpu --once          # 텍스트 스냅샷 (빠른 확인 / 로그용)
sgpu --json          # JSON 스냅샷 (스크립트용: sgpu --json | jq ...)
sgpu --waste         # 유휴/parked GPU 목록; 있으면 exit 1 — cron/알림용
sgpu --usage [일수]  # 유저별 GPU-hours + 효율 (기본 7일)
sgpu --wait-free 2 --partition heavy   # 빈 GPU 2개 생길 때까지 대기 후 exit 0
```

`--waste`를 cron+메일에 걸면 설정 없이 GPU 사재기 일일 다이제스트가 됩니다.
`--wait-free`로 자리 나는 순간 잡 제출 스크립트를 짤 수 있습니다.

### 화면 구성

```
▼ node01   ● idle   gpu_short   32/64   ████░░░░ 128/256G
               0   A100    ████████░  85%   █████░░  40/80G   72C   280W   eightmm  12345   2:30h
               1   A100    ░░░░░░░░░   0%   ░░░░░░░   0/80G   35C    45W
▼ node02   ○ alloc  heavy       12/64   ██░░░░░░  48/256G
               0   H100    ████████░  91%   ███████  64/80G   78C   400W   jaemin  67890   10:15h
```

- **노드 헤더 행** (어두운 초록 배경): 노드명, 상태, 파티션, CPU 할당/전체, RAM + GPU당 글리프 스트립 (`█` 사용 중 · `▅` parked · `▂` 예약-유휴 · `▁` 빈 GPU · `!` rogue)과 집계 — `Space`로 접으면 노드당 한 줄 클러스터 오버뷰
- **`user !gres` / `user !slurm` 마커 (빨강)**: 해당 GPU에 **SLURM 할당 없이** 프로세스 실행 중. `!gres` = 그 유저가 `--gres` 빼고 제출한 잡이 노드에 있음 (`w` 팝업에서 잡ID 연결), `!slurm` = SLURM 밖 생 프로세스. 둘 다 빨간 ROGUE 칩 + `--waste` 최상단. 시스템 데몬 제외 (`SLURM_GPU_TUI_ROGUE_IGNORE`, 기본 `root,gdm,xdm`)
- **FREE 칩** (요약바): 전체 빈 GPU 수와 위치
- **`parked` 배지**: VRAM만 잡고 연산 0% (메모리 점유 낭비)
- **GPU 행**: 헤더 아래 들여쓰기 — 사용률 바, VRAM, 온도, 전력, 유저, Job, 잔여 시간
- **상태 기호**: `●` idle · `◐` mixed · `○` alloc · `✖` drain
- **`user idle 3.2h` 마커**: SLURM이 해당 유저의 잡에 할당했지만 GPU 프로세스가 없는 상태 + 유휴 지속 시간 (1시간 넘으면 노란 강조 — 회수 후보)
- **오류 노드**: `~stale` 대신 구체적인 원인 표시 (예: `~timeout`, `~unreachable`, `~smi_err`)

---

## 구조

```
[sgpu-agent @ 각 노드]     ──3초──→  ~/.sgpu/nodes/<노드>.json   (공유 FS push)
                                          │
[sgpu-collector @ 마스터]  ──병합──→  /tmp/slurm-gpu-tui/data.json
                                          ↑
[sgpu TUI]                 ──읽기──┘   (즉시 로드, 실행 시 SSH 없음)
```

**Push 모드 (기본):** 각 GPU 노드에 상주하는 `sgpu-agent`가 자기 통계를 공유 파일시스템에 몇 초마다 기록합니다. 마스터의 collector는 이 파일을 로컬로 읽기만 하므로 핫패스에 SSH가 없고, sshd가 불안정하거나 노드가 바빠도 수집이 멈추지 않습니다.

**Self-healing:** collector가 에이전트를 자동 배포·수리합니다. 노드 파일이 오래되면(에이전트 죽음, 재부팅, 구버전) SSH로 재기동 — 노드별 rate-limit. 노드에 따로 설치할 것 없음(공유 venv 직접 실행).

**SSH pull 폴백:** 살아있는 에이전트가 없는 노드는 기존처럼 SSH로 수집(ControlMaster 풀, 노드별 비동기). 두 모드는 자유롭게 혼용됩니다.

TUI는 병합된 JSON을 매 갱신 시 읽으므로 클러스터 규모와 무관하게 즉시 시작됩니다. Collector가 없으면 TUI가 직접 SSH 수집으로 폴백합니다 (첫 로딩 느림).

---

## Collector 데몬 관리

### sudo 있는 경우 (시스템 서비스)

```bash
# 상태 확인
sudo systemctl status sgpu-collector

# 재시작
sudo systemctl restart sgpu-collector

# 실시간 로그
sudo journalctl -u sgpu-collector -f

# 최근 로그
sudo journalctl -u sgpu-collector --since "10 minutes ago"

# 중지 / 비활성화
sudo systemctl stop sgpu-collector
sudo systemctl disable sgpu-collector
```

### sudo 없는 경우 (유저 서비스)

```bash
# 상태 확인
systemctl --user status sgpu-collector

# 재시작
systemctl --user restart sgpu-collector

# 실시간 로그
journalctl --user -u sgpu-collector -f

# 중지 / 비활성화
systemctl --user stop sgpu-collector
systemctl --user disable sgpu-collector
```

### sudo 없는 경우 (백그라운드 프로세스)

```bash
# 실행 중인지 확인
pgrep -a -f sgpu-collector

# 로그 확인
tail -f /tmp/sgpu-collector.log

# 중지
pkill -f sgpu-collector
```

---

## 제거

한 줄 — collector·노드 에이전트 중지, 서비스·심링크·데이터·설치 디렉토리 전부 삭제:

```bash
curl -fsSL https://raw.githubusercontent.com/eightmm/slurm-gpu-tui/main/uninstall.sh | bash
```

<details>
<summary>수동 절차 (스크립트가 하는 일)</summary>

### sudo 있는 경우 (시스템 서비스)

```bash
sudo systemctl stop sgpu-collector
sudo systemctl disable sgpu-collector
sudo rm -f /etc/systemd/system/sgpu-collector.service
sudo rm -f /usr/local/bin/sgpu /usr/local/bin/sgpu-collector
sudo systemctl daemon-reload
rm -rf ~/.sgpu/app    # 또는 SGPU_INSTALL_DIR 경로
```

### sudo 없는 경우 (유저 서비스)

```bash
systemctl --user stop sgpu-collector
systemctl --user disable sgpu-collector
rm -f ~/.config/systemd/user/sgpu-collector.service
systemctl --user daemon-reload
# ~/.bashrc에서 PATH 줄 제거
rm -rf ~/.sgpu/app    # 또는 SGPU_INSTALL_DIR 경로
```

### 노드 에이전트 (공통)

```bash
# push 에이전트 중지 + 데이터 삭제
for n in $(sinfo -N -h -o %N | sort -u); do ssh "$n" 'pkill -f "bin/[s]gpu-agent"' 2>/dev/null; done
rm -rf ~/.sgpu/nodes
```

### sudo 없는 경우 (백그라운드 프로세스)

```bash
pkill -f sgpu-collector
# ~/.bashrc에서 nohup 줄 및 PATH 줄 제거
rm -rf ~/.sgpu/app    # 또는 SGPU_INSTALL_DIR 경로
```

</details>

---

## 트러블슈팅

**`sgpu` 명령을 못 찾는 경우**
```bash
# 래퍼 스크립트 확인
ls ~/.sgpu/app/bin/sgpu

# PATH 임시 적용
export PATH="$HOME/.sgpu/app/bin:$PATH"
```

**처음 실행 시 느린 경우 ("loading GPUs..." 메시지)**

Collector 데몬이 실행 중이지 않은 것입니다. 상태 확인 후 재시작하세요:
```bash
sudo systemctl status sgpu-collector      # 시스템 서비스
systemctl --user status sgpu-collector   # 유저 서비스
```

**노드에 `~timeout` 또는 `~unreachable` 표시**

해당 노드로 SSH 연결이 실패하는 것입니다:
```bash
ssh <노드명>        # 직접 접속 테스트
ssh -v <노드명>     # 상세 오류 확인
```

**노드에 `~smi_err` 또는 `~no_smi` 표시**

해당 노드에서 `nvidia-smi`가 동작하지 않는 것입니다:
```bash
ssh <노드명> nvidia-smi
```

**Collector가 계속 크래시되는 경우**
```bash
sudo journalctl -u sgpu-collector -n 50 --no-pager   # 시스템 서비스
journalctl --user -u sgpu-collector -n 50 --no-pager  # 유저 서비스
cat /tmp/sgpu-collector.log                            # 백그라운드
```

**재설치**
```bash
curl -fsSL https://raw.githubusercontent.com/eightmm/slurm-gpu-tui/main/bootstrap.sh | bash
```

---

## 환경 변수

| 변수 | 기본값 | 설명 |
|------|--------|------|
| `SLURM_GPU_TUI_REFRESH_SEC` | `3` | TUI 갱신 주기 (초) |
| `SLURM_GPU_TUI_COLLECTOR_SEC` | `3` | Collector 수집 주기 |
| `SLURM_GPU_TUI_NODE_TIMEOUT_SEC` | `30` | 노드 SSH 타임아웃 |
| `SLURM_GPU_TUI_MAX_WORKERS` | `8` | 병렬 SSH 워커 수 (폴백 모드) |
| `SLURM_GPU_TUI_DATA_DIR` | `/tmp/slurm-gpu-tui` | 데몬 JSON 출력 경로 |
| `SLURM_GPU_TUI_STATE_DIR` | `~/.sgpu/state` | 영속 상태 (usage 히스토리, 낭비 나이, 인벤토리) — 재부팅 유지 |
| `SLURM_GPU_TUI_AGENT_DIR` | `~/.sgpu/nodes` | push 에이전트 데이터 경로 (공유 FS) |
| `SLURM_GPU_TUI_AGENT_SEC` | `3` | 노드 에이전트 수집 주기 |
| `SLURM_GPU_TUI_AGENT_MAX_AGE_SEC` | `45` | 에이전트 데이터 신선도 한계 |
| `SLURM_GPU_TUI_AGENT_REPAIR_SEC` | `180` | 노드당 에이전트 수리 최소 간격 |
| `SLURM_GPU_TUI_AGENT_DISABLE` | (없음) | 설정 시 push 에이전트 비활성화 |
| `SLURM_GPU_TUI_WASTE_MIN_SEC` | `600` | 낭비 뷰 / `--waste` 임계값 |
| `SLURM_GPU_TUI_USAGE_KEEP_DAYS` | `30` | GPU-hour 히스토리 보존 기간 |
| `SLURM_GPU_TUI_ROGUE_IGNORE` | `root,gdm,xdm` | rogue로 안 잡을 유저 목록 |
| `SLURM_GPU_TUI_SHARE_SCRIPTS` | (없음) | collector가 전체 잡의 batch script를 공유 — 모든 유저가 Enter 팝업에서 봄. **스크립트 내용(비밀키 포함 가능)이 전원에게 공개됨.** 설치 시 `[Y/n]`으로 물어봄; `SGPU_SHARE_SCRIPTS=0/1`이면 질문 생략 |

---

## 요구 사항

- Python 3.10+
- SLURM 클러스터 (마스터 노드에서 `sinfo` / `squeue` 사용 가능)
- 마스터 노드 → 컴퓨트 노드 SSH 접속 가능 (비밀번호 없이)
- GPU 노드에 `nvidia-smi` 설치
