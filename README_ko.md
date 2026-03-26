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

### 한 줄 설치 (sudo 유무 자동 감지)

```bash
git clone https://github.com/eightmm/slurm-gpu-tui.git
cd slurm-gpu-tui
bash install.sh
```

`install.sh`가 환경을 자동으로 감지해서 전부 처리합니다:

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

> **설치 디렉토리를 옮기면?** `bash install.sh` 다시 실행하면 됩니다.

---

## 사용법

```bash
sgpu        # GPU 모니터 실행
```

### 키보드 단축키

| 키 | 동작 |
|----|------|
| `r` | 즉시 새로고침 |
| `f` | Fast(1초) / Normal(3초) 전환 |
| `s` | 정렬 순환: 노드명 → 사용률 → 유저 |
| `u` | 내 Job 필터 토글 (내 Job만 강조) |
| `i` | 유휴 필터 토글 (빈 GPU 있는 노드만 표시) |
| `d` | 상세 컬럼 토글 (온도 / 전력 / JobID / JobName) |
| `Space` | 노드 접기 / 펼치기 (노드 헤더 행에 커서 위치 필요) |
| `/` | 노드명 또는 유저명으로 검색 — `Esc`로 초기화 |
| `j` / `k` | 커서 아래 / 위 이동 (vim 스타일) |
| `e` | 현재 상태를 JSON 파일로 내보내기 |
| `q` | 종료 |

### 화면 구성

```
▼ node01   ● idle   gpu_short   32/64   ████░░░░ 128/256G
               0   A100    ████████░  85%   █████░░  40/80G   72C   280W   hklee   12345   2:30h
               1   A100    ░░░░░░░░░   0%   ░░░░░░░   0/80G   35C    45W
▼ node02   ○ alloc  heavy       12/64   ██░░░░░░  48/256G
               0   H100    ████████░  91%   ███████  64/80G   78C   400W   jaemin  67890   10:15h
```

- **노드 헤더 행** (어두운 초록 배경): 노드명, 상태 기호, 파티션, CPU 할당/전체, RAM
- **GPU 행**: 헤더 아래 들여쓰기 — 사용률 바, VRAM, 온도, 전력, 유저, Job, 잔여 시간
- **상태 기호**: `●` idle · `◐` mixed · `○` alloc · `✖` drain
- **오류 노드**: `~stale` 대신 구체적인 원인 표시 (예: `~timeout`, `~unreachable`, `~smi_err`)

---

## 구조

```
[sgpu-collector]  ──→  /tmp/slurm-gpu-tui/data.json
                              ↑
[sgpu TUI]        ──읽기──┘   (즉시 로드, 실행 시 SSH 없음)
```

Collector 데몬이 백그라운드에서 지속 실행되며, SLURM 명령어와 SSH로 각 GPU 노드를 주기적으로 수집합니다. TUI는 이 JSON 파일을 매 갱신 시 읽어들이므로 클러스터 규모에 관계없이 즉시 시작됩니다.

데몬이 실행 중이지 않으면 TUI가 직접 SSH 수집으로 폴백합니다 (첫 로딩 느림).

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

### sudo 있는 경우 (시스템 서비스)

```bash
sudo systemctl stop sgpu-collector
sudo systemctl disable sgpu-collector
sudo rm -f /etc/systemd/system/sgpu-collector.service
sudo rm -f /usr/local/bin/sgpu /usr/local/bin/sgpu-collector
sudo systemctl daemon-reload
rm -rf /path/to/slurm-gpu-tui
```

### sudo 없는 경우 (유저 서비스)

```bash
systemctl --user stop sgpu-collector
systemctl --user disable sgpu-collector
rm -f ~/.config/systemd/user/sgpu-collector.service
systemctl --user daemon-reload
# ~/.bashrc에서 PATH 줄 제거
rm -rf /path/to/slurm-gpu-tui
```

### sudo 없는 경우 (백그라운드 프로세스)

```bash
pkill -f sgpu-collector
# ~/.bashrc에서 nohup 줄 및 PATH 줄 제거
rm -rf /path/to/slurm-gpu-tui
```

> 설치 시 출력되는 제거 명령어를 복사해두면 편합니다.

---

## 트러블슈팅

**`sgpu` 명령을 못 찾는 경우**
```bash
# 래퍼 스크립트 확인
ls ~/slurm-gpu-tui/bin/sgpu

# PATH 임시 적용
export PATH="$HOME/slurm-gpu-tui/bin:$PATH"
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
bash install.sh    # 기존 venv와 서비스를 덮어씁니다
```

---

## 환경 변수

| 변수 | 기본값 | 설명 |
|------|--------|------|
| `SLURM_GPU_TUI_REFRESH_SEC` | `3` | TUI 갱신 주기 (초) |
| `SLURM_GPU_TUI_FAST_REFRESH_SEC` | `1` | Fast 모드 갱신 주기 |
| `SLURM_GPU_TUI_NODE_TIMEOUT_SEC` | `30` | 노드 SSH 타임아웃 |
| `SLURM_GPU_TUI_MAX_WORKERS` | `8` | 병렬 SSH 워커 수 (폴백 모드) |
| `SLURM_GPU_TUI_DATA_DIR` | `/tmp/slurm-gpu-tui` | 데몬 JSON 출력 경로 |

---

## 요구 사항

- Python 3.10+
- SLURM 클러스터 (마스터 노드에서 `sinfo` / `squeue` 사용 가능)
- 마스터 노드 → 컴퓨트 노드 SSH 접속 가능 (비밀번호 없이)
- GPU 노드에 `nvidia-smi` 설치
