"""Generates tiny synthetic models and reference data for BetterRecall.Worker.Tests. No downloads.

Run: python -I make_synthetic.py <out_dir>   (needs: onnx numpy sentencepiece pillow)
Model files use the .onnxt extension so *.onnx ignore rules skip nothing; tests copy them to .onnx.
Outputs (all static shapes, random weights with a fixed seed):
  sp.model                SentencePiece BPE with byte fallback (Gemma-style), vocab 320
  sp_expected.json        ids from the Python sentencepiece reference for a few strings
  embed_tokens.bf16       [320,32] bf16 token table; text_dyn.onnxt inputs_embeds [1,seq,32], attention_mask -> [1,768]
  text_16.onnx, text_32.onnx   input_ids, attention_mask -> embedding [1,768] (tanh of the masked mean of a table)
  vision_28.onnx          pixel_values [1,3,64,112] -> image_embeds [1,28,32] (patch conv)
  fusion_28.onnx          image_embeds, input_ids, attention_mask -> embedding [1,768]
  siglip_vision.onnx      pixel_values [1,3,64,64] -> embedding [1,768]
  nsfw.onnx               pixel_values [1,3,224,224] -> logits [1,2] (red-heavy image scores "nsfw")
  nsfw_detector.onnx      images [1,3,h,w] -> output0 [1,22,8], every row = mean green (NudeNet-shaped detector)
  vad.onnx                Silero-shaped: input [1,576], state [2,1,128], sr int64 [] -> output [1,1], stateN
  resize_ref.json         PIL bicubic/bilinear resize references for the C# resampler
"""
import json, os, sys
import numpy as np
import onnx
from onnx import helper as h, TensorProto as T, numpy_helper as nh

OPSET = [h.make_opsetid("", 18)]
rng = np.random.default_rng(1234)
D, V = 768, 320


def save(graph, path):
    m = h.make_model(graph, opset_imports=OPSET, producer_name="betterrecall-synthetic")
    m.ir_version = 9
    onnx.checker.check_model(m)
    onnx.save(m, path)


def init(name, arr):
    return nh.from_array(np.asarray(arr), name)


def text_graph(seq, table, path):
    nodes = [
        h.make_node("Gather", ["table", "input_ids"], ["emb"], axis=0),                  # [1,S,D]
        h.make_node("Cast", ["attention_mask"], ["maskf"], to=T.FLOAT),                 # [1,S]
        h.make_node("Unsqueeze", ["maskf", "ax2"], ["mask3"]),                           # [1,S,1]
        h.make_node("Mul", ["emb", "mask3"], ["masked"]),
        h.make_node("ReduceSum", ["masked", "ax1"], ["sum"], keepdims=0),               # [1,D]
        h.make_node("ReduceSum", ["maskf", "ax1"], ["cnt"], keepdims=1),                # [1,1]
        h.make_node("Max", ["cnt", "one"], ["cnt1"]),
        h.make_node("Div", ["sum", "cnt1"], ["mean"]),
        h.make_node("Tanh", ["mean"], ["embedding"]),
    ]
    g = h.make_graph(nodes, f"text_{seq}",
                     [h.make_tensor_value_info("input_ids", T.INT64, [1, seq]),
                      h.make_tensor_value_info("attention_mask", T.INT64, [1, seq])],
                     [h.make_tensor_value_info("embedding", T.FLOAT, [1, D])],
                     [init("table", table), init("ax1", np.array([1], np.int64)),
                      init("ax2", np.array([2], np.int64)), init("one", np.array([[1.0]], np.float32))])
    save(g, path)


def detector_graph(path):
    """NudeNet-shaped detector: images [1,3,h,w] (dynamic) -> output0 [1,22,8]; every row and anchor holds the mean
    of the green channel, so a green frame scores 1 and a red or blue one 0."""
    g = h.make_graph([
        h.make_node("GlobalAveragePool", ["images"], ["gp"]),                           # [1,3,1,1]
        h.make_node("Reshape", ["gp", "s3"], ["r"]),                                    # [1,3,1]
        h.make_node("Gather", ["r", "green"], ["gr"], axis=1),                          # [1,1,1]
        h.make_node("Expand", ["gr", "shape"], ["output0"]),                            # [1,22,8]
    ], "nsfw_detector", [h.make_tensor_value_info("images", T.FLOAT, [1, 3, "h", "w"])],
        [h.make_tensor_value_info("output0", T.FLOAT, [1, 22, 8])],
        [init("s3", np.array([1, 3, 1], np.int64)), init("green", np.array([1], np.int64)),
         init("shape", np.array([1, 22, 8], np.int64))])
    save(g, path)


def main(out):
    os.makedirs(out, exist_ok=True)
    import sentencepiece as spm
    corpus = os.path.join(out, "corpus.txt")
    lines = ["the quick brown fox jumps over the lazy dog", "a spreadsheet with quarterly sales figures",
             "Tabelle mit Umsatzzahlen und Diagrammen", "bảng doanh thu theo quý", "search result query title none text",
             "task: search result | query: cat on a mat", "title: none | text: meeting notes about the budget"] * 40
    with open(corpus, "w", encoding="utf-8") as f:
        f.write("\n".join(lines))
    spm.SentencePieceTrainer.train(input=corpus, model_prefix=os.path.join(out, "sp"), vocab_size=V,
                                   model_type="bpe", byte_fallback=True, character_coverage=1.0,
                                   pad_id=0, eos_id=1, bos_id=2, unk_id=3, add_dummy_prefix=False,
                                   normalization_rule_name="identity", remove_extra_whitespaces=False,
                                   split_digits=True, minloglevel=2)
    os.remove(corpus)
    os.remove(os.path.join(out, "sp.vocab"))
    sp = spm.SentencePieceProcessor(model_file=os.path.join(out, "sp.model"))
    samples = ["task: search result | query: cat on a mat", "title: none | text: Tabelle mit Umsatzzahlen",
               "bảng doanh thu", "emoji 🙂 and 12345", "  two  spaces"]
    with open(os.path.join(out, "sp_expected.json"), "w", encoding="utf-8") as f:
        json.dump([{"text": s, "ids": sp.encode(s)} for s in samples], f, ensure_ascii=False)

    table = rng.standard_normal((V, D)).astype(np.float32) * 0.5
    for seq in (16, 32):
        text_graph(seq, table, os.path.join(out, f"text_{seq}.onnxt"))

    # EmbeddingGemma-2 layout: bf16 token table outside the graph + one dynamic-length text graph over inputs_embeds.
    H = 32
    tbl = (rng.standard_normal((V, H)) * 0.5).astype(np.float32)
    bits = tbl.view(np.uint32)
    bf16 = ((bits + 0x7FFF + ((bits >> 16) & 1)) >> 16).astype(np.uint16)   # round to nearest even
    bf16.tofile(os.path.join(out, "embed_tokens.bf16"))
    wd = (rng.standard_normal((H, D)) / np.sqrt(H)).astype(np.float32)
    g = h.make_graph([
        h.make_node("Cast", ["attention_mask"], ["maskf"], to=T.FLOAT),
        h.make_node("Unsqueeze", ["maskf", "ax2"], ["mask3"]),
        h.make_node("Mul", ["inputs_embeds", "mask3"], ["masked"]),
        h.make_node("ReduceSum", ["masked", "ax1"], ["sum"], keepdims=0),
        h.make_node("ReduceSum", ["maskf", "ax1"], ["cnt"], keepdims=1),
        h.make_node("Max", ["cnt", "one"], ["cnt1"]),
        h.make_node("Div", ["sum", "cnt1"], ["mean"]),
        h.make_node("MatMul", ["mean", "wd"], ["mm"]),
        h.make_node("Tanh", ["mm"], ["embedding"]),
    ], "text_dyn", [h.make_tensor_value_info("inputs_embeds", T.FLOAT, [1, "seq", H]),
                    h.make_tensor_value_info("attention_mask", T.INT64, [1, "seq"])],
        [h.make_tensor_value_info("embedding", T.FLOAT, [1, D])],
        [init("wd", wd), init("ax1", np.array([1], np.int64)), init("ax2", np.array([2], np.int64)),
         init("one", np.array([[1.0]], np.float32))])
    save(g, os.path.join(out, "text_dyn.onnxt"))

    # vision: [1,3,64,112] --conv16--> [1,32,4,7] -> [1,32,28] -> [1,28,32]
    cw = (rng.standard_normal((32, 3, 16, 16)) / 16).astype(np.float32)
    g = h.make_graph([
        h.make_node("Conv", ["pixel_values", "cw"], ["c"], kernel_shape=[16, 16], strides=[16, 16]),
        h.make_node("Reshape", ["c", "shape"], ["r"]),
        h.make_node("Transpose", ["r"], ["image_embeds"], perm=[0, 2, 1]),
    ], "vision_28", [h.make_tensor_value_info("pixel_values", T.FLOAT, [1, 3, 64, 112])],
        [h.make_tensor_value_info("image_embeds", T.FLOAT, [1, 28, 32])],
        [init("cw", cw), init("shape", np.array([1, 32, 28], np.int64))])
    save(g, os.path.join(out, "vision_28.onnxt"))

    # fusion: mean(image_embeds) @ wi + masked mean of table[input_ids] -> tanh
    L = 32
    wi = (rng.standard_normal((32, D)) / np.sqrt(32)).astype(np.float32)
    g = h.make_graph([
        h.make_node("ReduceMean", ["image_embeds", "ax1"], ["im"], keepdims=0),            # [1,32]
        h.make_node("MatMul", ["im", "wi"], ["ip"]),                                      # [1,D]
        h.make_node("Gather", ["table", "input_ids"], ["emb"], axis=0),
        h.make_node("Cast", ["attention_mask"], ["maskf"], to=T.FLOAT),
        h.make_node("Unsqueeze", ["maskf", "ax2"], ["mask3"]),
        h.make_node("Mul", ["emb", "mask3"], ["masked"]),
        h.make_node("ReduceMean", ["masked", "ax1"], ["tm"], keepdims=0),
        h.make_node("Add", ["ip", "tm"], ["s"]),
        h.make_node("Tanh", ["s"], ["embedding"]),
    ], "fusion_28", [h.make_tensor_value_info("image_embeds", T.FLOAT, [1, 28, 32]),
                     h.make_tensor_value_info("input_ids", T.INT64, [1, L]),
                     h.make_tensor_value_info("attention_mask", T.INT64, [1, L])],
        [h.make_tensor_value_info("embedding", T.FLOAT, [1, D])],
        [init("wi", wi), init("table", table), init("ax1", np.array([1], np.int64)), init("ax2", np.array([2], np.int64))])
    save(g, os.path.join(out, "fusion_28.onnxt"))

    # siglip-like vision: [1,3,64,64] -> avgpool 8 -> flatten 192 -> matmul -> [1,768]
    ws = (rng.standard_normal((192, D)) / np.sqrt(192)).astype(np.float32)
    g = h.make_graph([
        h.make_node("AveragePool", ["pixel_values"], ["p"], kernel_shape=[8, 8], strides=[8, 8]),
        h.make_node("Reshape", ["p", "shape"], ["f"]),
        h.make_node("MatMul", ["f", "ws"], ["embedding"]),
    ], "siglip_vision", [h.make_tensor_value_info("pixel_values", T.FLOAT, [1, 3, 64, 64])],
        [h.make_tensor_value_info("embedding", T.FLOAT, [1, D])],
        [init("ws", ws), init("shape", np.array([1, 192], np.int64))])
    save(g, os.path.join(out, "siglip_vision.onnxt"))

    # nsfw: global average pool -> gemm; logit_nsfw = 8*(R - (G+B)/2) in normalised units
    wn = np.array([[0, 0, 0], [8, -4, -4]], np.float32)  # [2,3]
    g = h.make_graph([
        h.make_node("GlobalAveragePool", ["pixel_values"], ["gp"]),
        h.make_node("Flatten", ["gp"], ["fl"]),
        h.make_node("Gemm", ["fl", "wn"], ["logits"], transB=1),
    ], "nsfw", [h.make_tensor_value_info("pixel_values", T.FLOAT, [1, 3, 224, 224])],
        [h.make_tensor_value_info("logits", T.FLOAT, [1, 2])], [init("wn", wn)])
    save(g, os.path.join(out, "nsfw.onnxt"))
    detector_graph(os.path.join(out, "nsfw_detector.onnxt"))

    # vad: output = sigmoid(200*mean|x| - 4); stateN = state (Silero-shaped interface)
    g = h.make_graph([
        h.make_node("Abs", ["input"], ["a"]),
        h.make_node("ReduceMean", ["a", "ax1"], ["m"], keepdims=1),                     # [1,1]
        h.make_node("Mul", ["m", "k"], ["mk"]),
        h.make_node("Sub", ["mk", "b"], ["z"]),
        h.make_node("Sigmoid", ["z"], ["output"]),
        h.make_node("Identity", ["state"], ["stateN"]),
        h.make_node("Cast", ["sr"], ["srf"], to=T.FLOAT),
    ], "vad", [h.make_tensor_value_info("input", T.FLOAT, [1, 576]),
               h.make_tensor_value_info("state", T.FLOAT, [2, 1, 128]),
               h.make_tensor_value_info("sr", T.INT64, [])],
        [h.make_tensor_value_info("output", T.FLOAT, [1, 1]), h.make_tensor_value_info("stateN", T.FLOAT, [2, 1, 128])],
        [init("ax1", np.array([1], np.int64)), init("k", np.array(200.0, np.float32)), init("b", np.array(4.0, np.float32))])
    save(g, os.path.join(out, "vad.onnxt"))

    # PIL resize references
    from PIL import Image
    src = (rng.integers(0, 256, (37, 53, 3))).astype(np.uint8)
    refs = []
    for mode, flag in (("bicubic", Image.BICUBIC), ("bilinear", Image.BILINEAR)):
        for (w, hgt) in ((20, 14), (80, 60), (53, 10)):
            out_img = np.asarray(Image.fromarray(src).resize((w, hgt), flag))
            refs.append({"mode": mode, "w": w, "h": hgt, "rgb": out_img.reshape(-1).tolist()})
    with open(os.path.join(out, "resize_ref.json"), "w") as f:
        json.dump({"w": 53, "h": 37, "rgb": src.reshape(-1).tolist(), "refs": refs}, f)
    print("ok", out)


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else os.path.dirname(os.path.abspath(__file__)))
