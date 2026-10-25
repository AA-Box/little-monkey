"""CPU-only contract tests: no CUDA, GPU weights or remote downloads."""
import importlib.util
from pathlib import Path
import tempfile
import unittest

SOURCE = Path(__file__).resolve().parents[1] / "scripts" / "studio_vdn_disk_loader.py"
spec = importlib.util.spec_from_file_location("native_vdn_sharded_disk", SOURCE)
disk = importlib.util.module_from_spec(spec)
spec.loader.exec_module(disk)


class Component:
    def __init__(self, path="OpenVDN/vdn-minimax-h3", subfolder="h3-base/transformer"):
        self.pretrained_model_name_or_path = path
        self.subfolder = subfolder
        self.revision = "revision"


class Pipe:
    def __init__(self, mapping=None):
        self._component_specs = mapping or {
            "transformer": Component(),
            "text_encoder": Component("MiniMaxAI/MiniMax-H3", "text_encoder"),
        }


class NativeShardedDiskContracts(unittest.TestCase):
    def test_rejects_pickle_nonsharded_and_traversal(self):
        good = {"weight_map": {"blocks.0.attn.weight": "model-00001-of-00002.safetensors",
                               "blocks.1.ff.weight": "model-00002-of-00002.safetensors"}}
        self.assertEqual(disk.verify_safetensors_index(
            good, component="text_encoder")["shards"], 2)
        for bad in ({"weight_map": {}},
                    {"weight_map": {"tensor": "model.bin"}},
                    {"weight_map": {"tensor": "../model.safetensors"}},
                    {"weight_map": {"tensor": ".private.safetensors"}},
                    {"weight_map": {"tensor": "a\\b.safetensors"}}):
            with self.assertRaises(ValueError):
                disk.verify_safetensors_index(bad, component="denoiser")

    def test_qwen_source_is_from_actual_diffusers_model_spec(self):
        pipe = Pipe()
        self.assertEqual(disk.component_spec_source(pipe, "text_encoder"),
                         ("MiniMaxAI/MiniMax-H3", "text_encoder", "revision"))
        self.assertEqual(disk.component_spec_source(pipe, "transformer"),
                         ("OpenVDN/vdn-minimax-h3", "h3-base/transformer", "revision"))
        with self.assertRaises(ValueError):
            disk.component_spec_source(Pipe({"transformer": Component(subfolder="../unsafe")}),
                                       "transformer")

    def test_low_ram_budget_admits_disk_staging_but_not_unsupported_claims(self):
        g = disk.GIB
        b = disk.low_ram_disk_budget(12*g, 16*g, 190*g)
        self.assertGreater(b["cpu_reserve_bytes"], 8*g)
        self.assertGreaterEqual(b["cuda_activation_reserve_bytes"], 8*g)
        for args in ((6*g,16*g,200*g), (12*g,8*g,200*g),
                     (12*g,16*g,120*g)):
            with self.assertRaises(ValueError):
                disk.low_ram_disk_budget(*args)

    def test_component_loader_is_sharded_dispatch_at_initial_load(self):
        with tempfile.TemporaryDirectory() as d:
            b = disk.low_ram_disk_budget(12*disk.GIB, 16*disk.GIB, 190*disk.GIB)
            config = disk.disk_component_kwargs("transformer", Path(d).resolve(), b)
            self.assertEqual(config["device_map"]["text_encoder"], "auto")
            self.assertEqual(config["device_map"]["transformer"], "auto")
            self.assertEqual(config["low_cpu_mem_usage"], {
                "text_encoder": True, "transformer": True})
            self.assertEqual(config["use_safetensors"], {
                "text_encoder": True, "transformer": True})
            self.assertEqual(config["offload_state_dict"], {
                "text_encoder": True, "transformer": True})
            self.assertNotEqual(config["offload_folder"]["text_encoder"],
                                config["offload_folder"]["transformer"])
            self.assertTrue(Path(config["offload_folder"]["transformer"]).is_dir())

    def test_requires_real_disk_hooks_on_both_large_models(self):
        class Model:
            def __init__(self, devices):
                self.hf_device_map = devices
        class Pipeline:
            transformer = Model({"": "cpu", "blocks.1": "disk"})
            text_encoder = Model({"embedding": "cpu", "layers.1": "disk"})
        self.assertEqual(disk.assert_disk_mapped_components(
            Pipeline(), "transformer")["transformer"]["diskGroups"], 1)
        Pipeline.text_encoder = Model({"": "cpu"})
        with self.assertRaisesRegex(RuntimeError, "text_encoder"):
            disk.assert_disk_mapped_components(Pipeline(), "transformer")


if __name__ == "__main__":
    unittest.main()
