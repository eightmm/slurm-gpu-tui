<div align="center">

# sgpu

**Slurm GPU 클러스터를 터미널에서 한눈에.**

GPU · CPU · RAM · 작업 · 사용자 · 알림

[![CI](https://github.com/eightmm/slurm-gpu-tui/actions/workflows/test.yml/badge.svg?branch=main)](https://github.com/eightmm/slurm-gpu-tui/actions/workflows/test.yml)
[![Rust](https://img.shields.io/badge/Rust-1.93%2B-CE422B?logo=rust)](https://github.com/eightmm/slurm-gpu-tui/blob/main/Cargo.toml)
[![Linux](https://img.shields.io/badge/Linux-2D6A4F?logo=linux&logoColor=white)](#build-and-install)

[화면](#화면-예시) · [설치](#build-and-install) · [사용법](#사용법) · [가이드](#가이드) · [English](../README.md)

</div>

| 클러스터 현황 | 작업 확인 | 운영 지원 |
|---|---|---|
| 실시간 자원 막대·노드 지도·여유량 | Priority 정렬·사용자별 보기·로그·본인 작업 취소 | GPU 낭비·사용 이력·Slack 알림·Prometheus 지표 |

## 화면 예시

![클릭 정렬 헤더, 사용자 강조와 대기 이유를 표시한 Jobs](images/tui-jobs.png)

*예시 데이터로 렌더링한 120열 화면입니다.*

<details>
<summary>다른 화면: GPU · Users · Host · 80열 Jobs</summary>

![자원 막대와 노드별 GPU 상태를 표시한 대시보드](images/tui-gpu.png)

![사용자별 작업 자원과 공유 노드 카운터를 표시한 Users](images/tui-users.png)

![컨트롤러 자원·저장공간·해석 기준을 표시한 Host](images/tui-host.png)

![80열 터미널의 Jobs](images/tui-jobs-narrow.png)

</details>

## Build and install

```bash
curl -fsSL https://raw.githubusercontent.com/eightmm/slurm-gpu-tui/refs/heads/main/bootstrap.sh | sudo bash
```

마스터에서 root로 실행합니다. Linux·git·C 컴파일러가 필요하며 Rust는 필요 시
준비합니다. 다시 실행하면 기존 경로·상태·Slack 설정을 유지하며 업그레이드합니다.
일반 사용자는 `sgpu`를 실행하면 됩니다.

각 노드는 공유 파일시스템에 결과를 게시합니다. [노드 설정 가이드](PUSH.md)를 따라
설치하면 호환되는 agent는 이후 [자동 업데이트](PUSH.md#automatic-agent-updates)를 받습니다.

## 사용법

```bash
sgpu                    # 대화형 대시보드
chkgpu                  # 사용자 × 노드 할당 행렬
chkgpu --summary        # 노드 여유량과 대기 이유
sgpu me                 # 내 작업·사용량
sgpu users              # 사용자별 자원
sgpu host               # 컨트롤러 자원
sgpu usage 7 --daily     # GPU-hours
sgpu logs 12345 -e -f    # stderr 따라가기
sgpu doctor             # 수집 상태 진단
```

| 조작 | 동작 |
|---|---|
| `1`–`7` | GPU / CPU / Usage / Jobs / Attention / Users / Host |
| `↕` 클릭 · `s` / `S` | Jobs 헤더 정렬 · 정렬 순환 / 역순 |
| `u` / `p` / `m` | 사용자 / 파티션 / 강조한 계정 필터 |
| Jobs에서 `v` | 전체 / 실행 / 대기 |
| `/` / `0` | 검색 / 필터 초기화 |
| Enter / Tab | 상세 / 스크립트·로그 탭 |
| `J` / Esc | GPU·CPU·Users·Attention에서 관련 Jobs / 이전 화면 |
| `?` / `q` | 전체 도움말 / 종료 |

전체 명령은 `sgpu --help`에서 확인합니다. `sgpu --highlight-user alice`로 계정을
강조할 수 있으며 권한은 실행 계정을 따릅니다. `chkgpu`는 `NO_COLOR`와 `--color always` / `--color never`를 지원합니다.

마지막 탭·정렬·Jobs 상태는 `~/.sgpu/ui/preferences.json`에 저장합니다.
검색어·계정 필터·로그·임시 관련 화면은 현재 세션에만 유지합니다.

`?`는 결측, `~`는 부분 관측입니다. 오래된 데이터는 빈 자원으로 계산하지 않습니다.
Users의 network/load는 노드 공용 값이며 priority·여유량·예상 시작 시각은 실행 승인을
보장하지 않습니다. 자세한 기준은 [지표 해석](PUSH.md#resource-views)을 참고하세요.

## 가이드

| 가이드 | 내용 |
|---|---|
| [노드 설정·업그레이드](PUSH.md) | 공유 경로·서비스·설정·문제 해결 |
| [Slack 알림](ALERTS.md) | Bot 설정·일별 스레드·소유자 DM |
| [Prometheus 지표](METRICS.md) | Textfile 출력·알림 규칙 |

설치 기본값은 작업 로그 tail과 정제된 상세를 snapshot 독자에게 공개합니다.
`SGPU_SHARE_LOGS=0` / `SGPU_SHARE_JOB_DETAILS=0`으로 끌 수 있으며 스크립트는 선택 공개입니다.

검사: `bash scripts/check.sh` · [버그 제보](https://github.com/eightmm/slurm-gpu-tui/issues) · [기여](https://github.com/eightmm/slurm-gpu-tui/pulls) · [Star](https://github.com/eightmm/slurm-gpu-tui/stargazers)
