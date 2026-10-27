"""Load only reviewed AST definitions from immutable legacy sources.

No module imports/startup, networking, app state, model calls or real corpus data.
The actual managed backend and operation implementations run on temporary SQLite.
"""
from __future__ import annotations

import ast
import base64
import calendar
import copy
import hashlib
import json
import logging
import re
import sqlite3
import sys
import threading
import time
import types
import uuid
from contextlib import contextmanager
from pathlib import Path

READ_PINS = {
    'managed_storage.py': '19e0cad75e79781dc1ef41dd3b00ffa375f4306f1ed00a9e3f6d50528a1e3f7b',
    'workspace_ops.py': '038bceeac705cdce91f2eb7f464bf92a06fc9b6fa3bedd665d382530e63ac7dc',
    'workspace_files.py': '243767a922b73d0308aab17dd41eca30beb5e1c50a1f70a40e940f027030ef07',
    'corpus_store.py': '26ce8fc9a309b9400fa77d6ae3ed10043861d5a8317d6aa33e1f457e3fb03fc8',
    'dedicated_storage.py': 'f9d5fd3f25c28fa0c7db009245f9eea37fd8fa4d96a83da9c2dc5e2e416f8a67',
}
PINS = {
    **READ_PINS,
    'workspace_trash.py': '95215946bc4e96094d39a33cd2d72331ecac575c91a263900f3ff51ca38e5632',
    'corpus.py': '4b8809d3b7d4811723404cbc9a40fe4fe0743bc18bbb449a673ec711c48e0a87',
    'app.py': '6ca99d74ec637cf37d063b8d48e0ae2062c8508a778475e527d1f49708f4e553',
}


def compile_nodes(nodes, filename, namespace):
    future = ast.ImportFrom(module='__future__', names=[ast.alias(name='annotations')], level=0)
    module = ast.Module(body=[future, *copy.deepcopy(nodes)], type_ignores=[])
    exec(compile(ast.fix_missing_locations(module), filename, 'exec'), namespace)


def load(source, now):
    # Validate EVERY source before importing test dependencies, executing source
    # definitions, creating a synthetic database or returning any expected result.
    raw = {name: (source / 'diary/agent' / name).read_bytes() for name in PINS}
    for name, digest in PINS.items():
        if hashlib.sha256(raw[name]).hexdigest() != digest:
            raise ValueError('frozen source hash mismatch: ' + name)
    trees = {name: ast.parse(data, filename=name) for name, data in raw.items()}
    from fastapi import HTTPException

    ns = {
        '__name__': '_frozen_workspace_base',
        'Path': Path, 'sqlite3': sqlite3, 'hashlib': hashlib, 'time': time,
        'threading': threading, 'uuid': uuid, 'contextmanager': contextmanager,
        'HTTPException': HTTPException, 're': re, 'calendar': calendar,
        'base64': base64, 'json': json, 'logging': logging,
    }

    def extract(name, functions=(), classes=None):
        selected = []
        for original in trees[name].body:
            node = copy.deepcopy(original)
            if isinstance(node, (ast.FunctionDef, ast.ClassDef)) and node.name in functions:
                selected.append(node)
            elif isinstance(node, ast.ClassDef) and node.name in (classes or {}):
                node.body = [n for n in node.body
                             if isinstance(n, ast.FunctionDef) and n.name in classes[node.name]]
                selected.append(node)
        compile_nodes(selected, name, ns)

    extract('dedicated_storage.py', ('StorageUnavailable',))
    extract('managed_storage.py', ('digest', 'safe_key'), {
        'ManagedCorpusBackend': {'__init__', 'db', 'meta', 'set_meta', 'active', 'list_dir'}})
    extract('corpus_store.py', classes={'CorpusStore': {'__init__', '_join', 'index_path'}})
    extract('workspace_ops.py', ('OpError', 'rel_path', 'require_managed', '_tree_meta',
                                 '_kind', 'folder_version', 'stat'))
    extract('workspace_files.py', ('safe_path', '_modified', 'file_list'))

    def assignment(name, key):
        found = [n for n in trees[name].body if isinstance(n, ast.Assign)
                 and any(isinstance(t, ast.Name) and t.id == key for t in n.targets)]
        if len(found) != 1:
            raise ValueError('missing unique source constant: ' + key)
        return found[0]

    compile_nodes([assignment('workspace_files.py', 'MAX_FILES')], 'workspace_files.py', ns)
    constants = {}
    compile_nodes([assignment('workspace_files.py', 'MAX_FILE')], 'workspace_files.py', constants)
    compile_nodes([assignment('workspace_trash.py', 'PREFIX')], 'workspace_trash.py', constants)
    corpus = dict(ns)
    compile_nodes([assignment('corpus.py', 'MARKER_RE')], 'corpus.py', corpus)
    month = [n for n in trees['corpus_store.py'].body
             if isinstance(n, ast.FunctionDef) and n.name == 'month_name_pattern']
    if len(month) != 1:
        raise ValueError('missing actual locale-month helper')
    compile_nodes(month, 'corpus_store.py', corpus)

    package = types.ModuleType('_frozen_workspace_oracle')
    package.__path__ = []
    sys.modules[package.__name__] = package
    clock = types.SimpleNamespace(time=lambda: now, gmtime=time.gmtime,
                                  strftime=time.strftime, monotonic=time.monotonic)

    def build(name):
        module = types.ModuleType(package.__name__ + '.' + name.removesuffix('.py'))
        module.__dict__.update(ns)
        module.__dict__.update(__name__=package.__name__ + '.' + name.removesuffix('.py'),
                               __package__=package.__name__, time=clock,
                               MAX_FILE=constants['MAX_FILE'], TRASH_PREFIX=constants['PREFIX'],
                               MARKER_RE=corpus['MARKER_RE'],
                               month_name_pattern=corpus['month_name_pattern'])
        nodes = [n for n in trees[name].body
                 if isinstance(n, (ast.FunctionDef, ast.ClassDef, ast.Assign))]
        compile_nodes(nodes, name, module.__dict__)
        sys.modules[module.__name__] = module
        return module

    ops, trash = build('workspace_ops.py'), build('workspace_trash.py')
    # Only Trash's actual routes map ValueError to409. An unrelated operation
    # failing with ValueError must abort generation, never become an expected409.
    for name in ('api_workspace_trash_change', 'api_workspace_trash_list'):
        routes = [n for n in trees['app.py'].body
                  if isinstance(n, ast.FunctionDef) and n.name == name]
        if len(routes) != 1:
            raise ValueError('missing actual Trash route')
        handlers = [n for n in ast.walk(routes[0]) if isinstance(n, ast.ExceptHandler)
                    and isinstance(n.type, ast.Name) and n.type.id == 'ValueError']
        if len(handlers) != 1 or not any(
                isinstance(n, ast.Call) and isinstance(n.func, ast.Name)
                and n.func.id == 'HTTPException' and n.args
                and isinstance(n.args[0], ast.Constant) and n.args[0].value == 409
                for n in ast.walk(handlers[0])):
            raise ValueError('unexpected source Trash error mapping')
    return ns, ops, trash, HTTPException
