#!/usr/bin/env bash
# Summarise CRABTURE_PERF samples: n / median / p95 / max per metric and
# surface, in milliseconds. Reads the given logs (or stdin); lines that are
# not `crabture-perf <metric> <surface> <ms>` are ignored, so a whole stderr
# capture can be passed as-is. See docs/hotpath-profiling.md.
#
#   scripts/perf-summary.sh perf-*.log
set -euo pipefail

awk '$1 == "crabture-perf" && NF == 4 { print $2 " " $3 " " $4 }' "$@" |
	sort -k1,1 -k2,2 -k3,3g |
	awk '
	function flush() {
		if (n == 0) return
		# Nearest-rank percentiles.
		med = v[int(n * 50 / 100) + 1 > n ? n : int(n * 50 / 100) + 1]
		p95 = v[int(n * 95 / 100) + 1 > n ? n : int(n * 95 / 100) + 1]
		printf "%-36s %-6s n=%-4d median=%9.3fms p95=%9.3fms max=%9.3fms\n", m, f, n, med, p95, v[n]
	}
	{
		if ($1 != m || $2 != f) { flush(); m = $1; f = $2; n = 0 }
		v[++n] = $3
	}
	END { flush() }
	'
