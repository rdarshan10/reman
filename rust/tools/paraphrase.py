"""Casual search queries for each tldr description, written by a local open model, for training the
embedding model on how people actually ask (train_embedder.py --paraphrases).

  python rust/tools/paraphrase.py MODEL_DIR PAIRS.jsonl OUT.jsonl [--limit N] [--batch 32]

MODEL_DIR: an instruct model (Qwen2.5-1.5B-Instruct, Apache-2.0). PAIRS: build_cmd_pairs.py's
train.jsonl. OUT gets one {"tool", "desc", "queries": [...]} per description; a rerun skips
descriptions already done. Only tldr's own text (CC-BY) goes in, so what comes out is clean to
train on.
"""
import argparse, json, os, re, time

PROMPT = (
    "Command: {cmd}\nWhat it does: {desc}\n\n"
    "Write 3 different searches a developer might type to find this command again in their shell history. "
    "One plain, one as a question (\"how do i ...\"), one terse like a note to self. "
    "Lowercase, casual, no quotes, no numbering, no explanations. One per line."
)


def clean(line):
    line = re.sub(r"^\s*(?:[-*•]|\d+[.)])\s*", "", line).strip().strip("\"'`").strip()
    # the model labels its terse variant ("note to self: ...")
    line = re.sub(r"^(note to self|note|reminder|remember|terse|plain|question)\s*[:\-]\s*", "", line, flags=re.I)
    return line.strip().lower()


def tool_names():
    """Tool names that aren't everyday words: a name used as a word in 20+ tldr sentences (`find`,
    `make`, `file`, `kill`) says nothing about which tool a query means."""
    m = json.load(open(os.path.join(os.path.dirname(__file__), "..", "..", "tldr_map.json"), encoding="utf-8"))
    sentences = list(m["cmd"].values()) + [e[0] for exs in m["ex"].values() for e in exs]
    uses = {}
    for s in sentences:
        for w in set(re.findall(r"[a-z][a-z0-9_.+-]*", s.lower())):
            uses[w] = uses.get(w, 0) + 1
    return {k for k in m["cmd"] if "-" not in k and len(k) >= 3 and uses.get(k, 0) < 20}


def names_another_tool(query, r, tools):
    """The query names a tool the command doesn't use ("blender" for a 3d viewer's fps option):
    a pairing that would teach the wrong thing."""
    own = (r["cmd"] + " " + r["desc"]).lower()
    return any(w in tools and w not in own for w in re.findall(r"[a-z][a-z0-9_.+-]*", query))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("model")
    ap.add_argument("pairs")
    ap.add_argument("out")
    ap.add_argument("--limit", type=int, default=0)
    ap.add_argument("--batch", type=int, default=32)
    a = ap.parse_args()

    import torch
    from transformers import AutoModelForCausalLM, AutoTokenizer

    rows, seen = [], set()
    for line in open(a.pairs, encoding="utf-8"):
        r = json.loads(line)
        key = (r["tool"], r["desc"])
        if key not in seen:
            seen.add(key)
            rows.append(r)
    done = set()
    if os.path.exists(a.out):
        for line in open(a.out, encoding="utf-8"):
            r = json.loads(line)
            done.add((r["tool"], r["desc"]))
    todo = [r for r in rows if (r["tool"], r["desc"]) not in done]
    if a.limit:
        todo = todo[: a.limit]
    print(f"{len(rows)} descriptions, {len(done)} done, {len(todo)} to go", flush=True)

    tools = tool_names()
    tok = AutoTokenizer.from_pretrained(a.model, padding_side="left")
    model = AutoModelForCausalLM.from_pretrained(a.model, dtype=torch.float16, device_map="cuda").eval()
    t0 = time.time()
    with open(a.out, "a", encoding="utf-8") as out:
        for i in range(0, len(todo), a.batch):
            chunk = todo[i : i + a.batch]
            texts = [
                tok.apply_chat_template([{"role": "user", "content": PROMPT.format(cmd=r["cmd"], desc=r["desc"])}], tokenize=False, add_generation_prompt=True)
                for r in chunk
            ]
            enc = tok(texts, return_tensors="pt", padding=True).to("cuda")
            with torch.no_grad():
                gen = model.generate(**enc, max_new_tokens=64, do_sample=True, temperature=0.8, top_p=0.9, pad_token_id=tok.eos_token_id)
            for r, g in zip(chunk, gen[:, enc["input_ids"].shape[1] :]):
                lines = [clean(l) for l in tok.decode(g, skip_special_tokens=True).splitlines()]
                queries = [l for l in lines if 3 <= len(l) <= 120 and not names_another_tool(l, r, tools)][:3]
                if queries:
                    out.write(json.dumps({"tool": r["tool"], "desc": r["desc"], "queries": queries}, ensure_ascii=False) + "\n")
            out.flush()
            n = i + len(chunk)
            if n % (a.batch * 10) == 0 or n == len(todo):
                rate = n / (time.time() - t0)
                print(f"{n}/{len(todo)}  {rate:.1f}/s  ~{(len(todo) - n) / max(rate, 1e-9) / 60:.0f} min left", flush=True)


if __name__ == "__main__":
    main()
