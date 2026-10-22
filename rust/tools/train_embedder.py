"""Fine-tune reman's embedding model (bge-small-en-v1.5) on commands, and export it as ONNX in the
layout reman loads (model.onnx + tokenizer files).

  python rust/tools/train_embedder.py DATA_DIR OUT_DIR [--epochs 1] [--lr 3e-5] [--batch 128]
  python rust/tools/train_embedder.py --export-base OUT_DIR     # the untrained model, same export

DATA_DIR is build_cmd_pairs.py's output; --paraphrases FILE adds paraphrase.py's casual queries. Training uses train.jsonl only; 5% of its tools (by hash,
never the test tools) are held back to report progress on. test.jsonl is for embed_bench.rs.

Each example is (request, command, another command of the same tool): the request is the tldr
description, as it is or in reman's search-query form (reman embeds requests both ways); the
other command of the same tool is what teaches `git branch -d` from `git branch -a`. Loss: in-batch
negatives (MultipleNegativesRankingLoss) plus that hard negative.
"""
import argparse, hashlib, json, os, random, shutil

SEARCH_FORM = "Represent this sentence for searching relevant passages: "
BASE = "BAAI/bge-small-en-v1.5"


def rows(path):
    return [json.loads(l) for l in open(path, encoding="utf-8")]


def dev_tool(tool):
    return int(hashlib.sha256(("dev:" + tool).encode()).hexdigest(), 16) % 100 < 5


def sentence(r):
    """A description as reman embeds it (describe.rs): "git branch: Delete a local branch"."""
    return f"{r['page'].replace('-', ' ')}: {r['desc']}"


def examples(part, rnd, paraphrases=None):
    """(request, command, a same-tool command) from each description; with paraphrases, each
    casual query also to the command, and to the description sentence reman matches requests
    against (with a same-tool sentence as the wrong answer)."""
    by_tool = {}
    for r in part:
        by_tool.setdefault(r["tool"], []).append(r)
    form = lambda q: (SEARCH_FORM if rnd.random() < 0.5 else "") + q
    out = []
    for r in part:
        others = [o for o in by_tool[r["tool"]] if o["desc"] != r["desc"] and o["cmd"] != r["cmd"]]
        if not others:
            continue
        out.append({"anchor": form(r["desc"]), "positive": r["cmd"], "negative": rnd.choice(others)["cmd"]})
        for q in (paraphrases or {}).get((r["tool"], r["desc"]), []):
            out.append({"anchor": form(q), "positive": r["cmd"], "negative": rnd.choice(others)["cmd"]})
            out.append({"anchor": form(q), "positive": sentence(r), "negative": sentence(rnd.choice(others))})
    rnd.shuffle(out)
    return out


def export(model, out):
    """model.onnx (input_ids, attention_mask, token_type_ids -> last_hidden_state) + tokenizer."""
    import torch

    os.makedirs(out, exist_ok=True)
    tf = model[0].auto_model.eval().to("cpu")
    tok = model.tokenizer
    enc = tok(["export this example"], return_tensors="pt")
    args = (enc["input_ids"], enc["attention_mask"], enc["token_type_ids"])
    axes = {0: "batch", 1: "sequence"}

    class Wrap(torch.nn.Module):
        def __init__(self, m):
            super().__init__()
            self.m = m

        def forward(self, input_ids, attention_mask, token_type_ids):
            return self.m(input_ids=input_ids, attention_mask=attention_mask, token_type_ids=token_type_ids).last_hidden_state

    torch.onnx.export(
        Wrap(tf), args, os.path.join(out, "model.onnx"),
        input_names=["input_ids", "attention_mask", "token_type_ids"], output_names=["last_hidden_state"],
        dynamic_axes={"input_ids": axes, "attention_mask": axes, "token_type_ids": axes, "last_hidden_state": axes},
        opset_version=17, do_constant_folding=True, dynamo=False,
    )
    tmp = os.path.join(out, "_tok")
    tok.save_pretrained(tmp)
    tf.config.save_pretrained(tmp)
    for f in ("tokenizer.json", "tokenizer_config.json", "config.json"):
        shutil.copy(os.path.join(tmp, f), os.path.join(out, f))
    # newer transformers no longer write it; reman's loader (fastembed) reads it
    with open(os.path.join(out, "special_tokens_map.json"), "w", encoding="utf-8") as f:
        json.dump({k: str(v) for k, v in tok.special_tokens_map.items()}, f, indent=2)
    shutil.rmtree(tmp)
    print(f"exported -> {out}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("data", nargs="?")
    ap.add_argument("out")
    ap.add_argument("--export-base", action="store_true")
    ap.add_argument("--epochs", type=float, default=1)
    ap.add_argument("--lr", type=float, default=3e-5)
    ap.add_argument("--batch", type=int, default=128)
    ap.add_argument("--seed", type=int, default=7)
    ap.add_argument("--paraphrases", help="paraphrase.py's output: casual queries per description")
    a = ap.parse_args()

    from sentence_transformers import SentenceTransformer

    model = SentenceTransformer(BASE)
    if a.export_base:
        return export(model, a.out)

    import torch
    from datasets import Dataset
    from sentence_transformers import SentenceTransformerTrainer, SentenceTransformerTrainingArguments, losses
    from sentence_transformers.evaluation import InformationRetrievalEvaluator
    from sentence_transformers.training_args import BatchSamplers

    rnd = random.Random(a.seed)
    all_train = rows(os.path.join(a.data, "train.jsonl"))
    train = [r for r in all_train if not dev_tool(r["tool"])]
    dev = [r for r in all_train if dev_tool(r["tool"])]
    print(f"train {len(train)} pairs, dev {len(dev)} pairs ({len({r['tool'] for r in dev})} tools)")

    queries = {str(i): SEARCH_FORM + r["desc"] for i, r in enumerate(dev)}
    corpus = {str(i): r["cmd"] for i, r in enumerate(dev)}
    relevant = {str(i): {str(j) for j, o in enumerate(dev) if o["desc"] == r["desc"] and o["tool"] == r["tool"]} for i, r in enumerate(dev)}
    evaluator = InformationRetrievalEvaluator(queries, corpus, relevant, name="dev", accuracy_at_k=[1, 5], mrr_at_k=[10], show_progress_bar=False)
    print("before:", {k: round(v, 4) for k, v in evaluator(model).items() if "accuracy@1" in k or "mrr@10" in k})

    paraphrases = None
    if a.paraphrases:
        paraphrases = {}
        for line in open(a.paraphrases, encoding="utf-8"):
            p = json.loads(line)
            paraphrases[(p["tool"], p["desc"])] = p["queries"]
        print(f"paraphrases for {len(paraphrases)} descriptions")
    ds = Dataset.from_list(examples(train, rnd, paraphrases))
    print(f"{len(ds)} training examples")
    args = SentenceTransformerTrainingArguments(
        output_dir=os.path.join(a.out, "_ckpt"),
        num_train_epochs=a.epochs,
        per_device_train_batch_size=a.batch,
        learning_rate=a.lr,
        warmup_ratio=0.1,
        fp16=torch.cuda.is_available(),
        batch_sampler=BatchSamplers.NO_DUPLICATES,
        eval_strategy="no",
        save_strategy="no",
        logging_steps=50,
        seed=a.seed,
        report_to=[],
    )
    model.max_seq_length = 128
    trainer = SentenceTransformerTrainer(model=model, args=args, train_dataset=ds, loss=losses.MultipleNegativesRankingLoss(model))
    trainer.train()
    print("after:", {k: round(v, 4) for k, v in evaluator(model).items() if "accuracy@1" in k or "mrr@10" in k})
    shutil.rmtree(os.path.join(a.out, "_ckpt"), ignore_errors=True)
    export(model, a.out)


if __name__ == "__main__":
    main()
