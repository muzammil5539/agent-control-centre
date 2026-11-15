#!/usr/bin/env bash
# Simulates four agents reporting to a running Agent Control Centre.
U=http://127.0.0.1:7777
ev() { curl -s -X POST "$U/event" -d "$1" >/dev/null; }
ev '{"event":"agent.started","agent_id":"coding","name":"Coding Agent","task":"Refactoring authentication module","progress":5}'
ev '{"event":"agent.started","agent_id":"research","name":"Research Agent","task":"Researching LangGraph architectures","progress":10}'
ev '{"event":"agent.started","agent_id":"testing","name":"Testing Agent","task":"Running integration tests","progress":20}'
ev '{"event":"agent.started","agent_id":"browser","name":"Browser Agent","task":"Logging in to dashboard","progress":15}'
for p in 25 40 55 70; do
  sleep 1
  ev "{\"event\":\"agent.progress\",\"agent_id\":\"coding\",\"progress\":$p}"
  ev "{\"event\":\"agent.progress\",\"agent_id\":\"research\",\"progress\":$((p-10))}"
  ev "{\"event\":\"agent.progress\",\"agent_id\":\"testing\",\"progress\":$((p+10))}"
done
ev '{"event":"approval.required","agent_id":"browser","message":"Waiting for login approval","progress":60}'
sleep 2
ev '{"event":"agent.completed","agent_id":"testing","message":"142 tests passed"}'
ev '{"event":"agent.failed","agent_id":"research","message":"Rate limited by API"}'
echo "polling approval for browser agent:"
for i in $(seq 1 60); do
  r=$(curl -s $U/agents/browser); echo "$r"
  case "$r" in *'"approval":true'*|*'"approval":false'*) break;; esac
  sleep 1
done
