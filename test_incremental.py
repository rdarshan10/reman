import reman_daemon as D, time

def ms(req):
    t = time.time(); r = D.client(req); return (time.time() - t) * 1000, r

base, _ = ms({"op": "search", "query": "list kubernetes pods", "k": 3})
n0 = D.client({"op": "ping"})["indexed"]

# ingest a NEW command not in the seed
D.client({"op": "ingest", "command": "kubectl get pods --namespace prod", "exit": 0,
          "cwd": "D:/PlanetNaidu", "session": "s1", "actor": "agent:claude-code"})

post, r = ms({"op": "search", "query": "list kubernetes pods in production namespace", "k": 5})
n1 = D.client({"op": "ping"})["indexed"]

# ingest the SAME command again -> should update in place, not grow the index
D.client({"op": "ingest", "command": "kubectl get pods --namespace prod", "exit": 0,
          "cwd": "D:/PlanetNaidu", "session": "s2", "actor": "agent:claude-code"})
n2 = D.client({"op": "ping"})["indexed"]

found = any("kubectl get pods" in x["command"] for x in r.get("results", []))
print(f"baseline search:     {base:5.1f} ms")
print(f"post-ingest search:  {post:5.1f} ms   (incremental: should be ~baseline, NOT ~80ms rebuild)")
print(f"index size: {n0} -> {n1} (new cmd appended) -> {n2} (re-ingest same cmd: no growth)")
print(f"new command findable right after ingest: {found}")
for x in r.get("results", [])[:3]:
    print(f"   {x['command']}  sim={x['similarity']}")
