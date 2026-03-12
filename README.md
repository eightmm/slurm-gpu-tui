# slurm-gpu-tui (v0.1)

Slurm + GPU 사용 현황을 터미널에서 보는 읽기전용 TUI 프로토타입.

## 기능 (v0.3)
- 노드별 상태 + CPU Load + 메모리 정보
- 실행 중 Job 목록 (user/job/node/TRES)
- 사용자별 할당 GPU 합계
- GPU 메트릭(노드별): usage, VRAM used/total, power draw/cap, temperature, voltage(가능 시)
- GPU 프로세스 목록 (`nvidia-smi` 기반) + user/job 추정 컬럼
- 노드 병렬 수집(기본 8 workers)
- 기본 5초 자동 새로고침 + Fast 모드(2초)
- 스냅샷 Export(JSON/CSV)

## 실행
```bash
cd slurm-gpu-tui
python3 -m venv .venv
source .venv/bin/activate
pip install -r requirements.txt
python3 app.py
```

## Collector 데몬 (권장)
백그라운드에서 데이터를 미리 수집해 TUI 시작 시 즉시 표시:
```bash
python3 collector.py --daemon    # 데몬 시작
python3 app.py                   # 즉시 로드 (데몬 데이터 사용)

python3 collector.py --status    # 상태 확인
python3 collector.py --stop      # 데몬 중지
```
데몬 없이도 `app.py`는 직접 수집 모드로 정상 동작합니다.

## 환경 변수
- `SLURM_GPU_TUI_REFRESH_SEC` (기본: `5`)
- `SLURM_GPU_TUI_FAST_REFRESH_SEC` (기본: `2`)
- `SLURM_GPU_TUI_REMOTE` (기본: `1`)  
  - `1`: Slurm `srun -w <node>`로 각 노드에서 `nvidia-smi` 실행 (권장)
  - `0`: 로컬 노드에서만 조회
- `SLURM_GPU_TUI_NODE_TIMEOUT_SEC` (기본: `15`)
- `SLURM_GPU_TUI_MAX_WORKERS` (기본: `8`)
- `SLURM_GPU_TUI_EXPORT_DIR` (기본: `./exports`)
- `SLURM_GPU_TUI_DATA_DIR` (기본: `/tmp/slurm-gpu-tui`) — 데몬 데이터 파일 경로
- `SLURM_GPU_TUI_COLLECTOR_SEC` (기본: `5`) — 데몬 수집 주기

## 데이터 소스
- `sinfo -N -h -o "%N|%T|%c|%O|%m|%e|%G"`
- `squeue -h -t R -o "%i|%u|%P|%j|%M|%R|%b"`
- `nvidia-smi --query-gpu=index,name,uuid,utilization.gpu,memory.used,memory.total,power.draw,power.limit,temperature.gpu,voltage.graphics --format=csv,noheader,nounits`
- `nvidia-smi --query-compute-apps=gpu_uuid,pid,process_name,used_memory --format=csv,noheader,nounits`

## 단축키
- `r`: 즉시 새로고침
- `f`: Fast(2s) / Normal(기본 5s) 전환
- `e`: JSON 스냅샷 저장
- `c`: CSV 스냅샷 저장
- `q`: 종료

## 주의
- 클러스터별 Slurm format 출력이 다를 수 있어 파서 보정이 필요할 수 있음.
- GPU 프로세스의 user/job은 노드 내 단일 job일 때 정확, 다중 job 동시 실행 시 추정(`?`)으로 표시될 수 있음.
- 읽기전용이며 cancel/kill 기능 없음.
