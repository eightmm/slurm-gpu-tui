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

### sudo 권한이 있는 경우 (관리자 설치, 권장)

systemd 서비스로 Collector를 등록해 부팅 시 자동 시작하고, 서버의 모든 유저가 `sgpu`를 바로 쓸 수 있게 설정합니다.

```bash
git clone https://github.com/eightmm/slurm-gpu-tui.git
cd slurm-gpu-tui
bash install.sh
```

`install.sh`가 자동으로 처리합니다:
1. [uv](https://github.com/astral-sh/uv)로 Python 3.12 가상환경 생성 (없으면 자동 설치)
2. 패키지 설치
3. `bin/` 아래 래퍼 스크립트 생성
4. `sgpu-collector`를 systemd 서비스로 설치 및 시작 (sudo 필요)

설치 후 모든 유저가 쓸 수 있도록 심볼릭 링크 생성:

```bash
sudo ln -sf $(pwd)/bin/sgpu /usr/local/bin/sgpu
```

이후 모든 유저는 `sgpu`만 치면 됩니다. venv 활성화 필요 없음.

> **참고:** 설치 디렉토리를 옮기면 `bash install.sh` 재실행 후 심볼릭 링크를 다시 걸어주세요.

---

### sudo 권한이 없는 경우 (개인 설치)

sudo 없이 본인 계정에만 설치합니다.

#### 1단계 — 설치

```bash
git clone https://github.com/eightmm/slurm-gpu-tui.git
cd slurm-gpu-tui

# uv가 없으면 설치
curl -LsSf https://astral.sh/uv/install.sh | sh
export PATH="$HOME/.local/bin:$PATH"

# 가상환경 생성 및 패키지 설치
uv venv --python 3.12 .venv
uv pip install --python .venv/bin/python -e .
```

#### 2단계 — PATH 등록

```bash
# ~/.bashrc 또는 ~/.zshrc에 추가
echo 'export PATH="$HOME/slurm-gpu-tui/bin:$PATH"' >> ~/.bashrc
source ~/.bashrc
```

`$HOME/slurm-gpu-tui` 부분을 실제 클론 경로로 바꿔주세요.

#### 3단계 — Collector 데몬 시작

**방법 A: 백그라운드로 직접 실행**

```bash
nohup bin/sgpu-collector > /tmp/sgpu-collector.log 2>&1 &
```

로그인 시 자동 시작하려면 `~/.bashrc`나 시작 스크립트에 추가하세요.

**방법 B: systemd 유저 서비스로 등록** (시스템이 지원하는 경우)

```bash
# 실제 경로로 ExecStart 교체
sed "s|ExecStart=.*|ExecStart=$(pwd)/.venv/bin/sgpu-collector|" sgpu-collector.service \
  > ~/.config/systemd/user/sgpu-collector.service

systemctl --user daemon-reload
systemctl --user enable sgpu-collector
systemctl --user start sgpu-collector

# 상태 확인
systemctl --user status sgpu-collector
```

> **데몬 없이도 됩니다:** `sgpu`는 데몬 없이도 동작합니다. 다만 첫 실행 시 노드당 SSH 연결이 필요해 로딩이 느릴 수 있습니다.

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

- **노드 헤더 행** (어두운 초록 배경): 노드명, 상태, 파티션, CPU 할당/전체, RAM
- **GPU 행**: 헤더 아래 들여쓰기 — 사용률 바, VRAM, 온도, 전력, 유저, Job
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
