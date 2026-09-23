#!/usr/bin/env python3
"""
Bake-off harness: does an LLM-generated description recover the Phase-0 gate failures?

For each (command, gate_intent):
  1. ask an ollama model for a ONE-LINE description of the command
  2. embed description, raw command, and the intent (fastembed, same model as reman.py)
  3. compare cosine(intent, description) vs cosine(intent, raw_command)

A description "recovers the gate" if intent<->description similarity is clearly
higher than intent<->raw-command (which we know fails on these).

Watch for: verbose chain-of-thought (bad output shape), wrong descriptions
(hallucination POISONS retrieval), and latency.

USAGE:  python reman_llm_eval.py <ollama_model_tag>
"""
import sys, subprocess, time, shutil, os
import numpy as np
from fastembed import TextEmbedding

OLLAMA = shutil.which("ollama") or r"C:\Users\rdars\AppData\Local\Programs\Ollama\ollama.exe"
MODEL_EMBED = "BAAI/bge-small-en-v1.5"

# The three commands that FAILED the Phase-0 intent gate, with the intent that should match.
CASES = [
    ("alembic upgrade head",                                  "run database migrations"),
    ("docker-compose down -v",                                "tear down containers and wipe volumes"),
    ('scp -r "app" root@72.61.143.105:/opt/PlanetNaidu/',     "copy the app folder to the server over ssh"),
]

PROMPT = ("In ONE short line (max 12 words), describe what this shell command does. "
          "Output ONLY the description: no preamble, no reasoning, no markdown, no quotes.\n"
          "Command: {cmd}")

def gen(model, cmd):
    t0 = time.time()
    p = subprocess.run([OLLAMA, "run", model, PROMPT.format(cmd=cmd)],
                       capture_output=True, text=True, encoding="utf-8", errors="replace")
    dt = time.time() - t0
    out = (p.stdout or "").strip()
    return out, dt

def last_line(text):
    lines = [l.strip() for l in text.splitlines() if l.strip()]
    return lines[-1] if lines else ""

def cos(emb, a, b):
    va, vb = emb[a], emb[b]
    return float(va @ vb / (np.linalg.norm(va) * np.linalg.norm(vb)))

def main():
    model = sys.argv[1] if len(sys.argv) > 1 else "qwen3.5:2b"
    print(f"=== Bake-off: {model} ===  (ollama={OLLAMA})\n")
    embedder = TextEmbedding(model_name=MODEL_EMBED)

    rows = []
    for cmd, intent in CASES:
        raw_out, dt = gen(model, cmd)
        desc = last_line(raw_out)
        n_lines = len([l for l in raw_out.splitlines() if l.strip()])
        # embed the three texts
        texts = [intent, cmd, desc or " "]
        vs = list(embedder.embed(texts))
        emb = {"intent": vs[0], "raw": vs[1], "desc": vs[2]}
        sim_raw  = cos(emb, "intent", "raw")
        sim_desc = cos(emb, "intent", "desc")
        rows.append((cmd, intent, raw_out, desc, n_lines, dt, sim_raw, sim_desc))

    for cmd, intent, raw_out, desc, n_lines, dt, sim_raw, sim_desc in rows:
        print("-" * 78)
        print(f"COMMAND : {cmd}")
        print(f"INTENT  : {intent}")
        print(f"GEN     : {dt:.1f}s, {n_lines} output line(s)" + ("  <-- VERBOSE/CoT" if n_lines > 2 else ""))
        print(f"RAW OUT : {raw_out[:400]}" + (" ...[truncated]" if len(raw_out) > 400 else ""))
        print(f"DESC USED: {desc}")
        verdict = "RECOVERS" if sim_desc > sim_raw + 0.05 else ("no gain" if sim_desc <= sim_raw + 0.05 else "")
        print(f"SIM     : intent<->raw={sim_raw:.3f}   intent<->desc={sim_desc:.3f}   [{verdict}]")
    print("-" * 78)
    avg_raw  = np.mean([r[6] for r in rows])
    avg_desc = np.mean([r[7] for r in rows])
    avg_lat  = np.mean([r[5] for r in rows])
    print(f"AVG     : raw={avg_raw:.3f}  desc={avg_desc:.3f}  gain={avg_desc-avg_raw:+.3f}  | avg gen {avg_lat:.1f}s/cmd")

if __name__ == "__main__":
    main()
