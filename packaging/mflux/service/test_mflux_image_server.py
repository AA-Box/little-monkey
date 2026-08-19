from __future__ import annotations

import json
import threading
import time
import unittest
from http.client import HTTPConnection

from mflux_image_server import MfluxRunner, create_server


class FakeRegistry:
    def __init__(self):
        self.in_loop = []

    def register(self, callback):
        self.in_loop.append(callback)


class FakeImage:
    def save(self, stream, format=None):
        self.format = format
        stream.write(b"fake-png")


class FakeModel:
    def __init__(self, started, delay):
        self.callbacks = FakeRegistry()
        self.calls = 0
        self.started = started
        self.delay = delay

    def generate_image(self, **kwargs):
        self.calls += 1
        self.started.set()
        steps = kwargs["num_inference_steps"]
        for step in range(steps):
            for callback in list(self.callbacks.in_loop):
                callback.call_in_loop(step)
            time.sleep(self.delay)
        return FakeImage()


class MfluxServiceTest(unittest.TestCase):
    def setUp(self):
        self.models = []
        self.started = threading.Event()
        self.delay = 0.0

        def factory(**_kwargs):
            model = FakeModel(self.started, self.delay)
            self.models.append(model)
            return model

        self.server = create_server(
            "127.0.0.1",
            0,
            MfluxRunner("/tmp/model-source", "dev", 8, model_factory=factory),
        )
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.host, self.port = self.server.server_address

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=2)

    def request(self, method, path, body=None):
        connection = HTTPConnection(self.host, self.port, timeout=2)
        encoded = json.dumps(body).encode() if body is not None else None
        connection.request(method, path, encoded, {"Content-Type": "application/json"} if encoded else {})
        response = connection.getresponse()
        payload = json.loads(response.read())
        connection.close()
        return response.status, payload

    def wait_for(self, job_id):
        for _ in range(100):
            _, payload = self.request("GET", f"/sdcpp/v1/jobs/{job_id}")
            if payload["status"] in {"completed", "failed", "cancelled"}:
                return payload
            time.sleep(0.01)
        self.fail("job did not complete")

    def test_capabilities_and_warm_model(self):
        status, payload = self.request("GET", "/sdcpp/v1/capabilities")
        self.assertEqual(status, 200)
        self.assertEqual(payload["model"]["path"], "/tmp/model-source")
        self.assertTrue(payload["features"]["init_image"])
        request = {
            "prompt": "one",
            "width": 64,
            "height": 64,
            "seed": 1,
            "batch_count": 1,
            "sample_params": {"sample_steps": 2, "guidance": {"txt_cfg": 4}},
        }
        status, created = self.request("POST", "/sdcpp/v1/img_gen", request)
        self.assertEqual(status, 202)
        result = self.wait_for(created["id"])
        self.assertEqual(result["status"], "completed")
        self.assertEqual(len(result["result"]["images"]), 1)
        self.assertEqual(result["result"]["output_format"], "png")
        status, created = self.request("POST", "/sdcpp/v1/img_gen", request)
        self.assertEqual(status, 202)
        self.assertEqual(self.wait_for(created["id"])["status"], "completed")
        self.assertEqual(len(self.models), 1)
        self.assertEqual(self.models[0].calls, 2)

    def test_negative_prompt_is_rejected(self):
        status, payload = self.request(
            "POST",
            "/sdcpp/v1/img_gen",
            {"prompt": "one", "negative_prompt": "blur"},
        )
        self.assertEqual(status, 400)
        self.assertIn("negative", payload["error"])

    def test_active_cancellation_stops_sampling(self):
        self.delay = 0.02
        status, created = self.request(
            "POST",
            "/sdcpp/v1/img_gen",
            {
                "prompt": "one",
                "sample_params": {"sample_steps": 50},
            },
        )
        self.assertEqual(status, 202)
        self.assertTrue(self.started.wait(timeout=1))
        status, payload = self.request(
            "POST", f"/sdcpp/v1/jobs/{created['id']}/cancel"
        )
        self.assertEqual(status, 200)
        self.assertTrue(payload["cancelled"])
        self.assertEqual(self.wait_for(created["id"])["status"], "cancelled")


if __name__ == "__main__":
    unittest.main()
