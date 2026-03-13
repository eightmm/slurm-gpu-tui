# sgpu - SLURM GPU Monitor

터미널에서 SLURM 클러스터의 GPU 사용 현황을 실시간으로 확인하는 TUI 도구입니다.

![Python](https://img.shields.io/badge/python-3.10+-blue)

## 한눈에 보이는 것들

- 노드별 GPU 상태 (사용률, VRAM, 온도, 전력)
- CPU load / 메모리 사용량
- 누가 어떤 GPU를 쓰고 있는지
- 대기 중인 Job 목록
- 사용자별 GPU 할당 요약

---

## 설치

### 방법 1: 이미 설치된 서버에서 바로 사용

관리자가 이미 설치해뒀다면 아래 명령어만 치면 됩니다:

```bash
sgpu
```

끝입니다. 그냥 이것만 치면 GPU 현황이 뜹니다.

### 방법 2: 직접 설치

```bash
git clone https://github.com/eightmm/slurm-gpu-tui.git
cd slurm-gpu-tui
bash install.sh
```

설치 스크립트가 알아서 가상환경을 만들고 패키지를 설치합니다.
[uv](https://github.com/astral-sh/uv)를 사용하며, 없으면 자동으로 설치합니다 (`python3-venv` 불필요).

설치 후:

```bash
# 가상환경 활성화
source .venv/bin/activate

# 실행
sgpu
```

> **참고**: 전역 설치가 아닌 경우 가상환경 활성화 후에 `sgpu`를 사용할 수 있습니다.

---

## 사용법

```bash
sgpu                # GPU 모니터 실행
```

### 화면 안에서 쓸 수 있는 키

| 키 | 동작 |
|----|------|
| `r` | 즉시 새로고침 |
| `f` | 빠른 갱신(1초) / 보통(3초) 전환 |
| `e` | 현재 상태를 JSON 파일로 저장 |
| `q` | 종료 |

---

## Collector 데몬 (선택사항)

데몬을 띄워두면 `sgpu` 실행 시 데이터를 즉시 불러옵니다.
데몬 없이도 `sgpu`는 정상 동작하지만, 첫 로딩이 조금 느릴 수 있습니다.

```bash
sgpu-collector --daemon    # 백그라운드로 데몬 시작
sgpu-collector --status    # 돌고 있는지 확인
sgpu-collector --stop      # 데몬 중지
```

---

## 관리자용: 모든 유저가 쓸 수 있게 설치

모든 유저가 venv 활성화 없이 바로 `sgpu`를 쓸 수 있게 하려면:

```bash
git clone https://github.com/eightmm/slurm-gpu-tui.git
cd slurm-gpu-tui

# 설치 (가상환경 생성 + wrapper 생성 + /usr/local/bin에 복사)
sudo bash install.sh

# 데몬 띄우기 (모든 유저 공유)
sudo sgpu-collector --daemon
```

**`install.sh`가 하는 일:**

1. [uv](https://github.com/astral-sh/uv)가 없으면 자동 설치 (`python3-venv` 패키지 불필요)
2. `.venv` 생성 후 패키지 설치
3. `bin/` 에 wrapper 스크립트 생성
4. root로 실행 시: `/usr/local/bin`에 복사하여 모든 유저가 바로 사용 가능

이후 모든 유저는 `sgpu`만 치면 됩니다.

> **참고**: 설치 위치를 옮기면 `sudo bash install.sh`를 다시 실행하면 됩니다.

---

## 환경 변수 (고급)

기본값으로 충분하지만, 필요하면 조정할 수 있습니다:

| 변수 | 기본값 | 설명 |
|------|--------|------|
| `SLURM_GPU_TUI_REFRESH_SEC` | `3` | 화면 갱신 주기 (초) |
| `SLURM_GPU_TUI_FAST_REFRESH_SEC` | `1` | Fast 모드 갱신 주기 |
| `SLURM_GPU_TUI_COLLECTOR_SEC` | `3` | 데몬 수집 주기 |
| `SLURM_GPU_TUI_NODE_TIMEOUT_SEC` | `30` | 노드 SSH 타임아웃 |
| `SLURM_GPU_TUI_MAX_WORKERS` | `8` | 병렬 수집 워커 수 |

---

## 요구 사항

- Python 3.10+
- SLURM 클러스터 (`sinfo` / `squeue` 사용 가능해야 함)
- 마스터 노드에서 컴퓨트 노드로 SSH 접속 가능 (비밀번호 없이)
- GPU 노드에 `nvidia-smi` 설치되어 있어야 함
