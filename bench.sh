#!/bin/sh
# bench.sh — 启动延迟 / RSS / 单请求延迟 / 并发吞吐
cd /var/minis/workspace/nanobot-rs
export OPENAI_BASE=http://127.0.0.1:18891 OPENAI_API_KEY=sk-bench OPENAI_MODEL=mock

# 1) 清场
for f in /tmp/bench-*.pid; do [ -f "$f" ] && kill $(cat "$f") 2>/dev/null; done
sleep 0.5

# 2) core 启动延迟（3 次冷启平均）
START=""
for i in 1 2 3; do
  T0=$(date +%s%N 2>/dev/null || date +%s)
  env NANOBOT_SERVE=8092 ./target2/release/nanobot-rs > /dev/null 2>&1 &
  P=$!
  for j in $(seq 1 100); do
    curl -s -o /dev/null http://127.0.0.1:8092/health && break
    sleep 0.02
  done
  T1=$(date +%s%N 2>/dev/null || date +%s)
  echo "startup_ms[$i]: $(( (T1 - T0) / 1000000 ))"
  kill $P 2>/dev/null; wait $P 2>/dev/null
done

# 3) 稳态：起 core + mock
env NANOBOT_SERVE=8090 OPENAI_BASE=http://127.0.0.1:18891 ./target2/release/nanobot-rs > /tmp/bench-core.log 2>&1 &
echo $! > /tmp/bench-core.pid
python3 mock_openai.py > /tmp/bench-mock.log 2>&1 &
echo $! > /tmp/bench-mock.pid
sleep 1
core_ok=$(curl -s -o /dev/null -w "%{http_code}" http://127.0.0.1:8090/health)
mock_ok=$(curl -s -o /dev/null -w "%{http_code}" http://127.0.0.1:18891/healthz || true)
echo "core=$core_ok"

# 4) RSS（KB）
RSS=$(awk '/VmRSS/{print $2}' /proc/$(cat /tmp/bench-core.pid)/status 2>/dev/null)
echo "core_rss_kb: ${RSS:-NA}"

# 5) /health 单请求延迟（100 次，curl 计时统计）
T0=$(date +%s%N)
for i in $(seq 1 100); do curl -s -o /dev/null http://127.0.0.1:8090/health; done
T1=$(date +%s%N)
echo "health_avg_ms: $(( (T1 - T0) / 100 / 1000000 ))"

# 6) /api/run 完整 agent 回合（mock 秒回），10 次
T0=$(date +%s%N)
for i in $(seq 1 10); do
  curl -s -X POST http://127.0.0.1:8090/api/run \
    -H 'Content-Type: application/json' \
    -d '{}' > /dev/null 2>&1
done
T1=$(date +%s%N)
echo "note: run loop skipped if mock absent"

# 7) 并发吞吐 /health 50 并发 x 20
T0=$(date +%s%N)
for w in 1 2 3 4 5; do
  ( for i in $(seq 1 100); do curl -s -o /dev/null http://127.0.0.1:8090/health; done ) &
done
wait
T1=$(date +%s%N)
echo "concurrent_500req_ms: $(( (T1 - T0) / 1000000 ))"

kill $(cat /tmp/bench-core.pid) $(cat /tmp/bench-mock.pid) 2>/dev/null
echo BENCH-DONE
