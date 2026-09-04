"""chkgpu — one-shot user x node GPU/CPU matrix.

A deliberately separate, dependency-free view from the TUI: one screen of
"who is holding what right now", printed and gone. It reads slurm.conf and
squeue/sinfo directly rather than the collector snapshot, so it also works on
a host where sgpu is not installed as a service.

Entry point: the `chkgpu` console script, or `python -m sgpu.quickview`.
"""
import os
import re
import subprocess
from collections import defaultdict
from datetime import datetime
from functools import lru_cache


# ==============================================
# CONST
# ==============================================
NODE_W  = 8   # GPU 섹션 노드 컬럼 폭 (| 포함)
TOTAL_W = 9   # GPU 섹션 Total 컬럼 폭 (| 포함)
USER_W  = 13  # User 컬럼 폭 (| 포함)

C_HDR  = '\033[93m\033[40m'
C_WARN = '\033[91m\033[40m'
C_USER = '\033[3m\033[92m'
C_BOLD = '\033[1m'
C_RST  = '\033[0m'


def ncell(content, w=NODE_W):
    return content.rjust(w - 1) + '|'

def tcell(content, w=TOTAL_W):
    return content.rjust(w - 1) + '|'

def hdr(s, warn=False):
    return (C_WARN if warn else C_HDR) + s + C_RST


# ==============================================
# FUNC
# ==============================================
def readInputFile(f):
    with open(f, 'r') as fh:
        return fh.readlines()


def find_slurm_conf():
    """Locate slurm.conf: $SLURM_CONF, scontrol, then common package paths."""
    cand = [os.environ.get('SLURM_CONF', '')]
    try:
        out = subprocess.run(['scontrol', 'show', 'config'],
                             stdout=subprocess.PIPE, text=True, timeout=10).stdout
        for line in out.splitlines():
            if line.startswith('SLURM_CONF'):
                cand.append(line.split('=', 1)[1].strip())
    except Exception:
        pass
    cand += ['/etc/slurm/slurm.conf', '/etc/slurm-llnl/slurm.conf',
             '/usr/local/etc/slurm.conf', '/opt/slurm/etc/slurm.conf']
    for p in cand:
        if p and os.path.isfile(p) and os.access(p, os.R_OK):
            return p
    return None


def _expand_gpu_node_field(name_field):
    """Expand any Slurm NodeName, including a controller acting as slurmd."""
    return list(expand_hostlist(name_field))


def parse_gpu_nodes(slurm_cfg):
    """GPU-bearing NodeName lines -> (node names, GPUs per node)."""
    pairs = []
    for line in readInputFile(slurm_cfg):
        if not line.startswith('NodeName='):
            continue
        parts = line.split()
        names = _expand_gpu_node_field(parts[0].replace('NodeName=', ''))
        if not names:
            continue
        gres_part = next((p for p in parts if p.startswith('Gres=')), '')
        total = extract_gpu_count(gres_part.replace('Gres=', ''))
        if total <= 0:
            continue
        pairs.extend((name, total) for name in names)
    pairs.sort(key=lambda item: natural_sort_key(item[0]))
    return [n for n, _ in pairs], [g for _, g in pairs]


def parse_sinfo_nodes(sinfo_out):
    """Parse one ``sinfo -N`` result into status, partition and GPU inventory.

    Unlike the historical slurm.conf parser this accepts arbitrary node names,
    so a host named ``master`` can be both slurmctld and a GPU slurmd node.
    """
    node_status = {}
    node_partition = {}
    gpu_counts = {}
    for line in sinfo_out.splitlines():
        parts = line.strip().split('|')
        if len(parts) != 4:
            continue
        node, state, gres, partition = (part.strip() for part in parts)
        if not node:
            continue
        node = node.split('.')[0]
        node_status.setdefault(node, state.lower())
        node_partition.setdefault(node, partition.rstrip('*'))
        gpu_count = extract_gpu_count(gres)
        if gpu_count > 0:
            gpu_counts[node] = max(gpu_counts.get(node, 0), gpu_count)
    gpu_nodes = sorted(gpu_counts, key=natural_sort_key)
    return node_status, node_partition, gpu_nodes, [
        gpu_counts[node] for node in gpu_nodes
    ]


def extract_gpu_count(tres_str):
    return sum(int(m) for m in re.findall(
        r'(?:^|,)(?:gres/)?gpu(?::[^:,()]+)?:(\d+)(?=\(|,|$)',
        tres_str,
    ))


@lru_cache(maxsize=None)
def expand_hostlist(expr):
    """'gpu[1-3,5],node7' -> ('gpu1','gpu2','gpu3','gpu5','node7'), in Python.
    Forking `scontrol show hostnames` per job cost ~2N subprocesses per run."""
    hosts = []
    for part in re.findall(r'[^,\[\]]+(?:\[[^\]]*\])?', expr):
        m = re.match(r'^(.*?)\[([^\]]+)\]$', part)
        if not m:
            if part:
                hosts.append(part.split('.')[0])
            continue
        prefix, ranges = m.group(1), m.group(2)
        for r in ranges.split(','):
            if '-' in r:
                a, b = r.split('-', 1)
                width = len(a)
                hosts.extend(f'{prefix}{str(i).zfill(width)}'
                             for i in range(int(a), int(b) + 1))
            else:
                hosts.append(f'{prefix}{r}')
    return tuple(hosts)


def split_squeue(line):
    """Pipe-delimited squeue row -> stripped fields (whitespace splitting
    breaks on job names containing spaces)."""
    return [f.strip() for f in line.split('|')]


def aggregate_gpu_jobs(squeue):
    """GPU job 집계: squeue %b(TresPerNode) %e(EndTime) 필드 직접 사용,
    nodelist 확장으로 multi-node 정확 집계. 단일 패스, 서브프로세스 없음.
    Returns [(user, {node: gpus}, total, {node: earliest_end})] sorted."""
    per_user = {}

    for job in squeue:
        parts = split_squeue(job)
        if len(parts) < 13:
            continue
        user, state = parts[3], parts[4]
        if state != 'RUNNING':
            continue

        gres_str = parts[10]   # %b  TresPerNode
        end_str  = parts[11]   # %e  EndTime
        nodelist = parts[12]   # %R  NodeList

        if nodelist in ('(null)', 'N/A', 'None', ''):
            continue

        gpu_per_node = extract_gpu_count(gres_str)
        if gpu_per_node == 0:
            continue

        nodes = list(expand_hostlist(nodelist)) or [nodelist.split('.')[0]]

        u = per_user.setdefault(user, [defaultdict(int), 0, {}])
        for node in nodes:
            u[0][node] += gpu_per_node
            u[1] += gpu_per_node

        if end_str not in ('Unknown', 'N/A', 'None', 'NONE'):
            try:
                end_dt = datetime.strptime(end_str, '%Y-%m-%dT%H:%M:%S')
                for node in nodes:
                    if node not in u[2] or end_dt < u[2][node]:
                        u[2][node] = end_dt
            except ValueError:
                pass

    rows = [(user, usage, tot, ends) for user, (usage, tot, ends) in per_user.items() if tot]
    return sorted(rows, key=lambda x: (-x[2], x[0]))


def compute_thread_usage(squeue_lines):
    """squeue에서 노드별 CPU 스레드 사용량 계산"""
    user_node_cpu  = defaultdict(lambda: defaultdict(int))
    node_used_cpu  = defaultdict(int)
    user_total_cpu = defaultdict(int)

    for job in squeue_lines:
        parts = split_squeue(job)
        if len(parts) < 13 or parts[4] != 'RUNNING':
            continue
        user       = parts[3]
        cpus_str   = parts[7]
        nodelist   = parts[12]

        if not cpus_str.isdigit():
            continue
        total_cpus = int(cpus_str)
        if total_cpus == 0 or nodelist in ('(null)', 'N/A', ''):
            continue

        nodes = list(expand_hostlist(nodelist))
        if not nodes:
            continue

        n    = len(nodes)
        base = total_cpus // n
        rem  = total_cpus % n
        for i, node in enumerate(nodes):
            cpus = base + (1 if i < rem else 0)
            user_node_cpu[user][node] += cpus
            node_used_cpu[node]       += cpus
            user_total_cpu[user]      += cpus

    return user_node_cpu, node_used_cpu, user_total_cpu


def natural_sort_key(s):
    return [int(c) if c.isdigit() else c for c in re.split(r'(\d+)', s)]


def print_thread_section(title, nodes, users_sorted,
                          user_node_cpu, node_used_cpu, node_total_cpu,
                          min_col_w=8):
    """CPU 스레드 테이블 섹션 출력"""
    if not nodes:
        return

    col_w = {}
    for node in nodes:
        label = node.upper()
        col_w[node] = max(len(label) + 2, min_col_w)

    users_in_section = [u for u in users_sorted
                        if any(user_node_cpu[u].get(n, 0) > 0 for n in nodes)]

    grand_used  = sum(node_used_cpu.get(n, 0) for n in nodes)
    grand_total = sum(node_total_cpu.get(n, 0) for n in nodes)
    total_col_w = max(len(f'{grand_used}/{grand_total}') + 2,
                      len('Total') + 2, 9)

    print(f'\n{C_BOLD}─── {title} ───{C_RST}')

    # 헤더
    print(hdr('User'.rjust(USER_W - 1) + '|'), end='')
    for node in nodes:
        w = col_w[node]
        print(hdr(node.upper().rjust(w - 1) + '|'), end='')
    print('|' + hdr('Total'.rjust(total_col_w - 1) + '|'))

    # 유저별 행
    for user in users_in_section:
        user_total = sum(user_node_cpu[user].get(n, 0) for n in nodes)
        print(C_USER + user.rjust(USER_W - 2) + C_RST + ' |', end='')
        for node in nodes:
            cnt = user_node_cpu[user].get(node, 0)
            w   = col_w[node]
            print((str(cnt) if cnt > 0 else '').rjust(w - 1) + '|', end='')
        print('|' + (str(user_total) if user_total > 0 else '').rjust(total_col_w - 1) + '|')

    # Total 행
    print(C_BOLD + 'Total'.rjust(USER_W - 2) + C_RST + ' |', end='')
    for node in nodes:
        used  = node_used_cpu.get(node, 0)
        total = node_total_cpu.get(node, 0)
        w     = col_w[node]
        print(f'{used}/{total}'.rjust(w - 1) + '|', end='')
    print('|' + f'{grand_used}/{grand_total}'.rjust(total_col_w - 1) + '|')


# ==============================================
# MAIN
# ==============================================
def main():
    print(datetime.now().strftime('%Y/%m/%d %H:%M:%S'))

    # slurm.conf remains a fallback for installations whose sinfo omits GRES.
    SLURM_CFG = find_slurm_conf()

    # squeue / sinfo 병렬 호출 — 파이프 구분자: 공백 포함 잡 이름에도 안전.
    # argv lists, not shell=True: the format strings are full of % and | that
    # only survived because they were quoted for a shell that no longer runs.
    proc_sq = subprocess.Popen(
        ['squeue', '--noheader', '-o', '%i|%P|%j|%u|%T|%M|%D|%C|%p|%Q|%b|%e|%R'],
        stdout=subprocess.PIPE, text=True)
    proc_si = subprocess.Popen(['sinfo', '-N', '--noheader', '-o', '%n|%T|%G|%P'],
                               stdout=subprocess.PIPE, text=True)
    proc_cpu = subprocess.Popen(['sinfo', '-Nho', '%N %C'],
                                stdout=subprocess.PIPE, text=True)

    squeue_out, _ = proc_sq.communicate()
    sinfo_out, _ = proc_si.communicate()
    cpuinfo_out, _ = proc_cpu.communicate()
    squeue = squeue_out.split('\n')

    node_status, node_partition, gpu_nodes, gpu_nums = parse_sinfo_nodes(
        sinfo_out,
    )
    if not gpu_nodes and SLURM_CFG:
        gpu_nodes, gpu_nums = parse_gpu_nodes(SLURM_CFG)
    if not gpu_nodes and not SLURM_CFG:
        print(f'{C_WARN}GPU nodes not found in sinfo; slurm.conf not found '
              f'(set $SLURM_CONF){C_RST}')

    # 노드별 전체 CPU 수
    node_total_cpu = {}
    for line in cpuinfo_out.splitlines():
        parts = line.strip().split()
        if len(parts) < 2:
            continue
        node    = parts[0]
        val_str = parts[1].split('/')[-1] if '/' in parts[1] else parts[1]
        if val_str.isdigit():
            node_total_cpu[node] = int(val_str)

    # Pending job 집계: {user: {partition: count}}
    pending_by_user = defaultdict(lambda: defaultdict(int))
    for job in squeue:
        parts = split_squeue(job)
        if len(parts) > 4 and parts[4] == 'PENDING':
            pending_by_user[parts[3]][parts[1]] += 1

    # GPU job 집계 — 단일 패스 (nodelist 확장이 순수 파이썬이라 병렬 불필요)
    ret = aggregate_gpu_jobs(squeue)

    tota_usage    = 0
    node_usage    = defaultdict(int)
    all_end_times = {}
    for i in ret:
        for node in gpu_nodes:
            cnt = int(i[1].get(node, 0))
            tota_usage       += cnt
            node_usage[node] += cnt
        for nk, end_dt in i[3].items():
            if nk not in all_end_times or end_dt < all_end_times[nk]:
                all_end_times[nk] = end_dt

    # CPU 스레드 집계
    user_node_cpu, node_used_cpu, user_total_cpu = compute_thread_usage(squeue)

    # ==================== GPU 섹션 ====================
    # (tota_usage above is the running total actually reported; a second
    # total_used sum here was dead code, invisible while this file sat
    # outside the linter's reach)
    total_all  = sum(gpu_nums)

    # 노드 컬럼 폭: 노드명 기준 (파티션명은 폭에 맞게 축약 — veryshort 같은
    # 접미사 때문에 표가 옆으로 터지는 것 방지)
    _node_col_w = max(
        max((len(n.upper()) for n in gpu_nodes), default=4),
        7,
    ) + 2  # 양쪽 여백

    def gcell(content):
        return ncell(content, _node_col_w)

    def fmt_part(part, w=None):
        """파티션명을 컬럼 폭에 맞춤: 안 맞으면 '_' 앞 접두어, 그래도 길면 '…'."""
        fit = (w or _node_col_w) - 2
        if len(part) <= fit:
            return part
        prefix = part.split('_')[0]
        if len(prefix) <= fit:
            return prefix
        return prefix[:fit - 1] + '…'

    print(f'{C_BOLD}─── GPU Usage ───{C_RST}')

    # 파티션 행 (헤더 위에 표시)
    print(hdr('Part'.rjust(USER_W - 1) + '|'), end='')
    for node in gpu_nodes:
        part  = fmt_part(node_partition.get(node, ''))
        state = node_status.get(node, '')
        warn  = any(s in state for s in ('down', 'drain'))
        print(hdr(gcell(part), warn=warn), end='')
    print('|' + hdr(tcell('')))

    # 헤더 행
    print(hdr('User'.rjust(USER_W - 1) + '|'), end='')
    for node in gpu_nodes:
        state = node_status.get(node, '')
        warn  = any(s in state for s in ('down', 'drain'))
        mark  = '!' if warn else ''
        print(hdr(gcell(f'{node.upper()}{mark}'), warn=warn), end='')
    print('|' + hdr(tcell('Total')))

    # 유저별 행
    for i in ret:
        user      = i[0]
        gpu_total = i[2]

        print(C_USER + user.rjust(USER_W - 2) + C_RST + ' |', end='')
        for node in gpu_nodes:
            cnt = i[1].get(node, 0)
            print(gcell(str(cnt) if cnt > 0 else ''), end='')
        print('|' + tcell(str(gpu_total) if gpu_total > 0 else ''))

    # Total 행
    print(C_BOLD + 'Total'.rjust(USER_W - 2) + C_RST + ' |', end='')
    for idx, node in enumerate(gpu_nodes):
        used  = node_usage.get(node, 0)
        total = gpu_nums[idx]
        print(gcell(f'{used}/{total}'), end='')
    print('|' + tcell(f'{tota_usage}/{total_all}'))

    # Next free 행
    now = datetime.now()
    if any(n in all_end_times for n in gpu_nodes):
        print('Next free'.rjust(USER_W - 2) + ' |', end='')
        for node in gpu_nodes:
            nk = node
            if nk in all_end_times and node_usage.get(node, 0) > 0:
                secs = max(0, int((all_end_times[nk] - now).total_seconds()))
                d, rem = divmod(secs, 86400)
                h = rem // 3600
                m = (rem % 3600) // 60
                label = f'{d}d{h}h' if d > 0 else f'{h}h{m:02d}m'
            else:
                label = ''
            print(gcell(label), end='')
        print('|' + tcell(''))

    # ==================== CPU 스레드 섹션 ====================
    gpu_node_set = set(gpu_nodes)
    gpu_thr_nodes = sorted([n for n in node_total_cpu if n in gpu_node_set],
                            key=natural_sort_key)
    cpu_nodes     = sorted([n for n in node_total_cpu
                            if n not in gpu_node_set and n.startswith('cpu')],
                            key=natural_sort_key)
    other_nodes   = sorted([n for n in node_total_cpu
                             if n not in gpu_node_set and not n.startswith('cpu')],
                            key=natural_sort_key)

    # GPU 사용량 순서 기준으로 thread 섹션도 동일하게 정렬
    # GPU 잡 없는 유저는 뒤에 CPU 사용량 내림차순으로 추가
    _gpu_user_order = [i[0] for i in ret]
    _gpu_user_set   = set(_gpu_user_order)
    _cpu_only = sorted(
        (u for u in user_node_cpu if u not in _gpu_user_set),
        key=lambda u: (-user_total_cpu[u], u)
    )
    users_sorted = _gpu_user_order + _cpu_only

    if gpu_thr_nodes:
        print_thread_section('CPU Threads — GPU nodes', gpu_thr_nodes,
                              users_sorted, user_node_cpu, node_used_cpu, node_total_cpu,
                              min_col_w=_node_col_w)
    if cpu_nodes:
        print_thread_section('CPU Threads — CPU nodes', cpu_nodes,
                              users_sorted, user_node_cpu, node_used_cpu, node_total_cpu)
    if other_nodes:
        print_thread_section('CPU Threads — Other nodes', other_nodes,
                              users_sorted, user_node_cpu, node_used_cpu, node_total_cpu)

    # ==================== Pending 상세 ====================
    if pending_by_user:
        print(f'\n{C_BOLD}─── Pending Jobs ───{C_RST}')
        for user in sorted(pending_by_user.keys(), key=lambda u: (-sum(pending_by_user[u].values()), u)):
            pend_total = sum(pending_by_user[user].values())
            if pend_total == 0:
                continue
            breakdown  = ', '.join(f'{p}:{c}' for p, c in
                                   sorted(pending_by_user[user].items(), key=lambda x: -x[1]))
            print(C_USER + user.rjust(USER_W - 2) + C_RST + f':  {pend_total:3d}  ({breakdown})')

    # ==================== 클러스터 요약 ====================
    gpu_pct = round(tota_usage / total_all * 100) if total_all else 0
    total_cpu_used  = sum(node_used_cpu.values())
    total_cpu_all   = sum(node_total_cpu.values())
    cpu_pct = round(total_cpu_used / total_cpu_all * 100) if total_cpu_all else 0
    table_w = USER_W + _node_col_w * len(gpu_nodes) + 1 + TOTAL_W
    print(f'\n{"─" * table_w}')
    print(f'{C_BOLD}  GPU {tota_usage}/{total_all} ({gpu_pct}%)   CPU {total_cpu_used}/{total_cpu_all} ({cpu_pct}%){C_RST}')


if __name__ == '__main__':
    main()
