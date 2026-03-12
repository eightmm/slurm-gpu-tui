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

# 가상환경 만들고 설치
python3 -m venv .venv
source .venv/bin/activate
pip install -e .

# 실행
sgpu
```

> **참고**: 가상환경 활성화(`source .venv/bin/activate`) 후에 `sgpu` 명령어를 사용할 수 있습니다.

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

```bash
# 1. 설치 (아무 계정에서)
git clone https://github.com/eightmm/slurm-gpu-tui.git
cd slurm-gpu-tui
python3 -m venv .venv
.venv/bin/pip install -e .

# 2. 모든 유저가 쓸 수 있도록 wrapper 복사 (root 필요)
sudo cp bin/sgpu /usr/local/bin/sgpu
sudo cp bin/sgpu-collector /usr/local/bin/sgpu-collector
sudo chmod +x /usr/local/bin/sgpu /usr/local/bin/sgpu-collector

# 3. 데몬 띄우기 (root로 한 번만 - 모든 유저 공유)
sudo sgpu-collector --daemon
```

이후 모든 유저는 `sgpu`만 치면 바로 사용 가능합니다.

> **주의**: `bin/sgpu` 안에 설치 경로가 하드코딩되어 있습니다.
> 설치 위치를 바꾸면 `bin/sgpu`, `bin/sgpu-collector` 안의 경로도 수정해주세요.

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
