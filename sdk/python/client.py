"""Little Monkey Private Developer API -- Python client.

Standard-library only (``urllib``) -- this repo has no existing Python
dependency convention to follow, so no third-party HTTP library (e.g.
``requests``) is assumed. Copy this file into your own project; it isn't
published to PyPI.

See ./README.md for scopes and the auth header format.
"""

from __future__ import annotations

import json
import urllib.error
import urllib.request
from dataclasses import dataclass
from typing import Any, Dict, Iterator, List, Optional
from urllib.parse import quote


class LittleMonkeyApiError(Exception):
    """Raised for any non-2xx response. Carries the HTTP status and the
    parsed JSON body (or raw text if it wasn't JSON)."""

    def __init__(self, message: str, status: int, body: Any) -> None:
        super().__init__(message)
        self.status = status
        self.body = body


@dataclass
class LittleMonkeyClient:
    """One instance per token. Every method issues exactly one request; none
    retries or caches."""

    base_url: str
    token: Optional[str] = None
    timeout_seconds: float = 30.0

    def __post_init__(self) -> None:
        self.base_url = self.base_url.rstrip("/")

    def _headers(self, has_body: bool) -> Dict[str, str]:
        headers: Dict[str, str] = {}
        if has_body:
            headers["Content-Type"] = "application/json"
        if self.token:
            headers["Authorization"] = f"Bearer {self.token}"
        return headers

    def _request(
        self,
        method: str,
        path: str,
        body: Optional[Dict[str, Any]] = None,
        root_relative: bool = False,
    ) -> Any:
        origin = self.base_url
        if root_relative and origin.endswith("/v1"):
            origin = origin[: -len("/v1")]
        data = json.dumps(body).encode("utf-8") if body is not None else None
        request = urllib.request.Request(
            f"{origin}{path}",
            data=data,
            headers=self._headers(body is not None),
            method=method,
        )
        try:
            with urllib.request.urlopen(request, timeout=self.timeout_seconds) as response:
                raw = response.read().decode("utf-8")
                return json.loads(raw) if raw else None
        except urllib.error.HTTPError as error:
            raw = error.read().decode("utf-8")
            try:
                parsed = json.loads(raw) if raw else None
            except json.JSONDecodeError:
                parsed = raw
            raise LittleMonkeyApiError(
                f"{method} {path} failed with {error.code}", error.code, parsed
            ) from error

    def health(self) -> Dict[str, Any]:
        """``GET /health`` -- unauthenticated liveness probe, at the server
        root rather than under ``/v1``."""
        return self._request("GET", "/health", root_relative=True)

    def models(self) -> Dict[str, Any]:
        """``GET /v1/models``."""
        return self._request("GET", "/models")

    def chat(self, model: str, messages: List[Dict[str, str]], **extra: Any) -> Dict[str, Any]:
        """``POST /v1/chat/completions`` -- requires the ``chat`` scope.
        Always sent non-streaming; see :meth:`chat_stream` for incremental
        output."""
        body = {"model": model, "messages": messages, **extra, "stream": False}
        return self._request("POST", "/chat/completions", body)

    def chat_stream(
        self, model: str, messages: List[Dict[str, str]], **extra: Any
    ) -> Iterator[Dict[str, Any]]:
        """``POST /v1/chat/completions`` with ``stream: true`` -- yields each
        parsed SSE ``data:`` payload as it arrives. Requires the ``chat``
        scope."""
        body = {"model": model, "messages": messages, **extra, "stream": True}
        request = urllib.request.Request(
            f"{self.base_url}/chat/completions",
            data=json.dumps(body).encode("utf-8"),
            headers=self._headers(True),
            method="POST",
        )
        with urllib.request.urlopen(request, timeout=self.timeout_seconds) as response:
            for raw_line in response:
                line = raw_line.decode("utf-8").strip()
                if not line.startswith("data:"):
                    continue
                payload = line[len("data:"):].strip()
                if payload == "[DONE]":
                    return
                yield json.loads(payload)

    def knowledge_query(
        self,
        stack_id: str,
        query: str,
        *,
        query_id: Optional[str] = None,
        excluded_source_ids: Optional[List[str]] = None,
        rerank: Optional[bool] = None,
        token_budget: Optional[int] = None,
    ) -> Dict[str, Any]:
        """``POST /v1/knowledge/query`` -- requires the ``knowledge`` scope."""
        body: Dict[str, Any] = {"stack_id": stack_id, "query": query}
        if query_id is not None:
            body["query_id"] = query_id
        if excluded_source_ids is not None:
            body["excluded_source_ids"] = excluded_source_ids
        if rerank is not None:
            body["rerank"] = rerank
        if token_budget is not None:
            body["token_budget"] = token_budget
        return self._request("POST", "/knowledge/query", body)

    def workflow_run_status(self, run_id: str) -> Optional[Dict[str, Any]]:
        """``GET /v1/workflows/runs/{id}`` -- read-only run status. Requires
        the ``workflow_run`` scope. There is deliberately no method to
        *submit* a new run over this API."""
        return self._request("GET", f"/workflows/runs/{quote(run_id, safe='')}")

    def artifact_read(self, artifact_id: str) -> Dict[str, Any]:
        """``GET /v1/artifacts/{id}`` -- requires the ``artifact_read`` scope."""
        return self._request("GET", f"/artifacts/{quote(artifact_id, safe='')}")
