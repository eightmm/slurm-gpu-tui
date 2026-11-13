<div align="center">

# sgpu

**Slurm GPU 클러스터를 터미널에서 한눈에.**

작업·GPU 낭비·사용량·알림을 함께 확인하는 Rust 모니터입니다.

[![CI](https://github.com/eightmm/slurm-gpu-tui/actions/workflows/test.yml/badge.svg?branch=main)](https://github.com/eightmm/slurm-gpu-tui/actions/workflows/test.yml)
[![Rust](https://img.shields.io/badge/Rust-1.93%2B-CE422B?logo=rust)](https://github.com/eightmm/slurm-gpu-tui/blob/main/Cargo.toml)
[![Linux](https://img.shields.io/badge/platform-Linux-2D6A4F?logo=linux&logoColor=white)](https://github.com/eightmm/slurm-gpu-tui#build-and-install)
[![GitHub stars](https://img.shields.io/github/stars/eightmm/slurm-gpu-tui?style=social)](https://github.com/eightmm/slurm-gpu-tui/stargazers)

[설치](#build-and-install) · [사용법](#사용법) · [Slack 알림](ALERTS.md) · [English](../README.md)

</div>

## 주요 기능

| 기능 | 제공 내용 |
|---|---|
| **클러스터 상태** | GPU·CPU 텔레메트리, 실행·대기 작업, 하드웨어 상태 |
| **낭비 탐지와 집계** | idle·parked·rogue GPU 탐지와 사용자별 GPU-hours |
| **작업 들여다보기** | 이력·스크립트·로그와 본인 작업 취소 확인 |
| **화면 밖에서도 알림** | Slack 일별 스레드, 소유자 전용 실패 DM, Prometheus textfile |
| **노드가 결과 게시** | 독립 publisher로 수집하며, 결측·stale을 가용량과 구분 |

## Build and install

```bash
curl -fsSL https://raw.githubusercontent.com/eightmm/slurm-gpu-tui/refs/heads/main/bootstrap.sh | sudo bash
```

마스터에 root 권한으로 설치합니다. Linux·git·C 컴파일러가 필요하며, Rust가
없으면 설치기가 준비합니다. 첫 빌드는 수 분 걸릴 수 있습니다. 다시 실행하면
업그레이드하며, 기존 설치 경로·서비스 설정·상태·Slack 설정은 유지합니다.
설치 후 `sgpu --version`으로 **2.0.1**인지 확인할 수 있습니다. 일반 사용자는
`sgpu`만 실행하면 됩니다.

각 노드의 publisher와 공유 디렉터리는 [노드 설정](PUSH.md)을 따릅니다.
노드가 공유 파일시스템에 결과를 게시하고 마스터가 읽습니다. 마스터에서
노드 수집·설치·복구를 요청하는 경로는 없습니다.

## 사용법

```bash
sgpu                    # GPU / CPU / Usage / Jobs 화면
sgpu once               # 현재 상태
sgpu me                 # 내 작업·낭비·사용량
sgpu waste -v           # 낭비 GPU
sgpu usage 7 --daily     # GPU-hours
sgpu jobs 7              # 작업 이력
sgpu logs 12345 -e -f    # stderr 따라가기
sgpu doctor             # 수집·설정 진단
chkgpu                  # 사용자 × 노드 할당량
```

| 키 | 동작 |
|---|---|
| `1`–`4` | GPU / CPU / Usage / Jobs |
| `u` / `p` / `m` / `i` | 사용자 / 파티션 / 내 작업 / 빈 GPU 필터 |
| `/` / `s` / `S` | 검색 / 정렬 / 역순 |
| Enter / Tab | 상세 열기 / 정보·스크립트·로그 탭 전환 |
| `w` / `f` / `h` | 낭비 / 가용량 / 작업 이력 |
| `n` / `x` | 작업 watch / 본인 작업 취소 확인 |
| `?` / `q` | 전체 도움말 / 종료 |

`?`는 결측, `~`는 부분 관측입니다. 오래된 데이터는 빈 GPU로 계산하지 않으며,
가용량과 대기 시작 시각은 scheduler의 실행 승인을 보장하지 않습니다.

## 알림과 운영

[Slack 알림](ALERTS.md)은 유지됩니다. 일별 스레드, 장애·복구·낭비·GPU
건강 알림과 소유자에게만 전달하는 작업 실패 DM을 지원합니다. 토큰은 서버의
비공개 설정에 두며 저장소나 서비스 unit에 기록하지 않습니다.

root collector는 작업 로그와 정제된 상세를 기본 공개합니다. 클러스터 정책에
따라 `SGPU_SHARE_LOGS=0`, `SGPU_SHARE_JOB_DETAILS=0`으로 해제할 수 있고,
스크립트 공개는 선택 사항입니다. 로그·스크립트에는 민감한 내용이 들어갈 수
있으므로 공개 범위를 확인하세요.

- [노드 설치·설정·업그레이드](PUSH.md)
- [Slack 설정과 알림 종류](ALERTS.md)
- [Prometheus 메트릭](METRICS.md)

운영 스크립트는 `scripts/`, 구현은 `src/`, 검증은 `tests/`에 모았습니다.
`bash scripts/check.sh`로 형식·clippy·테스트·쉘 검사를 실행합니다.

## 프로젝트 응원하기

클러스터 운영에 도움이 됐다면 [Star](https://github.com/eightmm/slurm-gpu-tui)를
눌러 저장해 두세요. 버그 제보는 [Issues](https://github.com/eightmm/slurm-gpu-tui/issues),
개선 사항은 [Pull requests](https://github.com/eightmm/slurm-gpu-tui/pulls)로 받습니다.
