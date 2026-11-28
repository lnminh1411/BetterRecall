# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) BetterRecall contributors
"""Generate self-contained HTML pages and 960x540 PNG screenshots for MockAgent demo captures.

One HTML page is generated per window title in MockAgent.Apps (assets/demo/src/<slug>.html),
then optionally rendered via headless Microsoft Edge at 1280x720 and downscaled to 960x540
PNGs (assets/demo/<slug>.png) alongside assets/demo/index.json.

usage: python Scripts/demo_pages.py [--only slug,...] [--html-only]
"""
from __future__ import annotations

import argparse
import json
import os
import pathlib
import shutil
import subprocess
import sys
import tempfile
import time
from typing import Callable

ROOT = pathlib.Path(__file__).resolve().parent.parent
DEMO_DIR = ROOT / "assets" / "demo"
SRC_DIR = DEMO_DIR / "src"
INDEX_PATH = DEMO_DIR / "index.json"

EDGE_CANDIDATES = [
    pathlib.Path(r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe"),
    pathlib.Path(r"C:\Program Files\Microsoft\Edge\Application\msedge.exe"),
]

# ---------------- shared HTML chrome ----------------

BASE_CSS = """
* { box-sizing: border-box; margin: 0; padding: 0; }
html, body {
  width: 1280px; height: 720px; overflow: hidden;
  font-family: "Segoe UI", system-ui, -apple-system, sans-serif;
  font-size: 13px; line-height: 1.45;
  -webkit-font-smoothing: antialiased;
}
.mono { font-family: "Cascadia Mono", "Consolas", monospace; }
.win-controls { display: flex; align-items: center; gap: 2px; margin-left: auto; }
.win-btn {
  width: 38px; height: 30px; display: flex; align-items: center; justify-content: center;
  font-size: 11px; opacity: 0.78; user-select: none;
}
.win-btn.close:hover { background: #c42b1c; color: #fff; }
"""


def _page(title: str, extra_css: str, body: str) -> str:
    return f"""<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>{title}</title>
<style>
{BASE_CSS}
{extra_css}
</style>
</head>
<body>
{body}
</body>
</html>
"""


def _win_controls(dark: bool = False) -> str:
    color = "#d4d4d4" if dark else "#333333"
    return (
        f'<div class="win-controls" style="color:{color}">'
        '<div class="win-btn">&#x2014;</div>'
        '<div class="win-btn">&#x25A1;</div>'
        '<div class="win-btn close">&#x2715;</div>'
        '</div>'
    )


def _browser_shell(tab_title: str, url: str, content: str, extra_css: str = "", accent: str = "#1a73e8") -> str:
    css = f"""
body {{ background: #f3f5f8; color: #1f2328; display: flex; flex-direction: column; }}
.b-tabs {{
  height: 36px; background: #dee3ea; display: flex; align-items: flex-end;
  padding: 0 0 0 10px; gap: 4px;
}}
.b-tab {{
  height: 30px; min-width: 240px; max-width: 310px; background: #ffffff;
  border-radius: 8px 8px 0 0; padding: 0 12px; display: flex; align-items: center; gap: 8px;
  font-size: 12px; font-weight: 500; color: #1f2328;
  box-shadow: 0 -1px 2px rgba(0,0,0,0.06);
}}
.b-tab.inactive {{ background: transparent; box-shadow: none; color: #525c68; min-width: 150px; }}
.b-dot {{ width: 10px; height: 10px; border-radius: 50%; background: {accent}; flex-shrink: 0; }}
.b-tab-x {{ margin-left: auto; font-size: 10px; opacity: 0.55; }}
.b-bar {{
  height: 40px; background: #ffffff; border-bottom: 1px solid #dde2e8;
  display: flex; align-items: center; padding: 0 14px; gap: 10px;
}}
.b-nav {{ display: flex; gap: 10px; color: #5f6368; font-size: 14px; }}
.b-url {{
  flex: 1; height: 28px; background: #f1f3f4; border-radius: 14px;
  display: flex; align-items: center; padding: 0 14px; gap: 8px;
  font-size: 12.5px; color: #3c4043;
}}
.b-lock {{ color: #188038; font-size: 11px; }}
.b-avatar {{
  width: 26px; height: 26px; border-radius: 50%; background: {accent};
  color: #fff; font-size: 11px; font-weight: 600;
  display: flex; align-items: center; justify-content: center;
}}
.b-body {{ flex: 1; display: flex; overflow: hidden; }}
{extra_css}
"""
    body = f"""
<div class="b-tabs">
  <div class="b-tab"><span class="b-dot"></span><span>{tab_title}</span><span class="b-tab-x">&#x2715;</span></div>
  <div class="b-tab inactive"><span>New Tab</span><span class="b-tab-x">&#x2715;</span></div>
  {_win_controls(False)}
</div>
<div class="b-bar">
  <div class="b-nav"><span>&#x2190;</span><span>&#x2192;</span><span>&#x21BB;</span></div>
  <div class="b-url"><span class="b-lock">&#x1F512;</span><span>{url}</span></div>
  <div class="b-avatar">U</div>
</div>
<div class="b-body">{content}</div>
"""
    return _page(tab_title, css, body)


# ---------------- 1. Mail: Acme invoice 4471 ----------------

def build_mail_invoice() -> str:
    css = """
.mail-nav {
  width: 224px; background: #f6f8fc; border-right: 1px solid #e3e7ee;
  padding: 14px 12px; display: flex; flex-direction: column; gap: 4px;
}
.mail-brand { font-size: 17px; font-weight: 700; color: #1f2937; padding: 2px 8px 10px; display: flex; align-items: center; gap: 8px; }
.mail-logo { width: 22px; height: 22px; border-radius: 6px; background: #d93025; color: #fff; display: inline-flex; align-items: center; justify-content: center; font-size: 12px; }
.compose-btn {
  background: #c2e7ff; color: #001d35; font-weight: 600; border-radius: 14px;
  padding: 10px 18px; margin-bottom: 10px; width: 132px; text-align: center;
  box-shadow: 0 1px 2px rgba(0,0,0,0.08);
}
.nav-item { padding: 7px 14px; border-radius: 16px; display: flex; justify-content: space-between; color: #374151; font-size: 13px; }
.nav-item.active { background: #d3e3fd; color: #0b57d0; font-weight: 600; }
.mail-main { flex: 1; background: #ffffff; display: flex; flex-direction: column; }
.mail-top {
  height: 52px; border-bottom: 1px solid #eaedf2; padding: 0 24px;
  display: flex; align-items: center; gap: 16px;
}
.mail-search {
  width: 480px; height: 34px; background: #eaf1fb; border-radius: 18px;
  padding: 0 16px; display: flex; align-items: center; color: #5f6368; font-size: 13px;
}
.mail-content { padding: 22px 36px; overflow: hidden; }
.mail-subject { font-size: 21px; font-weight: 600; color: #1f1f1f; margin-bottom: 14px; display: flex; align-items: center; gap: 10px; }
.tag-inbox { font-size: 11px; background: #e8eaed; color: #3c4043; padding: 2px 8px; border-radius: 4px; font-weight: 500; }
.sender-row { display: flex; align-items: center; gap: 12px; margin-bottom: 18px; }
.sender-av { width: 38px; height: 38px; border-radius: 50%; background: #1a73e8; color: #fff; font-weight: 700; display: flex; align-items: center; justify-content: center; font-size: 15px; }
.sender-meta { flex: 1; }
.sender-name { font-weight: 600; font-size: 14px; color: #1f1f1f; }
.sender-addr { color: #5f6368; font-size: 12px; margin-left: 6px; font-weight: 400; }
.sender-to { color: #5f6368; font-size: 12px; }
.sender-date { color: #5f6368; font-size: 12px; }
.inv-card {
  border: 1px solid #dce1e8; border-radius: 10px; padding: 20px 24px;
  background: #fbfcfe; max-width: 740px;
}
.inv-head { display: flex; justify-content: space-between; align-items: baseline; border-bottom: 1px solid #e4e8ef; padding-bottom: 12px; margin-bottom: 14px; }
.inv-title { font-size: 18px; font-weight: 700; color: #111827; }
.inv-badge { background: #fef3c7; color: #92400e; font-size: 11.5px; font-weight: 600; padding: 3px 10px; border-radius: 12px; }
.inv-table { width: 100%; border-collapse: collapse; margin: 12px 0 16px; font-size: 13px; }
.inv-table th { text-align: left; color: #6b7280; font-weight: 600; padding: 6px 8px; border-bottom: 1px solid #e5e7eb; }
.inv-table td { padding: 8px; border-bottom: 1px solid #f0f2f5; }
.inv-table td.num, .inv-table th.num { text-align: right; }
.inv-summary { display: flex; justify-content: space-between; align-items: center; background: #eef4ff; padding: 12px 16px; border-radius: 8px; margin-top: 8px; }
.inv-amount { font-size: 17px; font-weight: 700; color: #0b57d0; }
.inv-pay { margin-top: 14px; font-size: 12.5px; color: #4b5563; display: flex; align-items: center; gap: 10px; }
.pay-pill { background: #e5e7eb; color: #374151; padding: 3px 10px; border-radius: 4px; font-family: "Cascadia Mono", monospace; font-size: 12px; }
.mail-actions { display: flex; gap: 10px; margin-top: 18px; }
.btn-outline { border: 1px solid #c7ccd4; background: #fff; color: #374151; padding: 7px 18px; border-radius: 18px; font-weight: 500; font-size: 13px; }
"""
    content = """
<div class="mail-nav">
  <div class="mail-brand"><span class="mail-logo">&#x2709;</span><span>Mail</span></div>
  <div class="compose-btn">&#x270E; Compose</div>
  <div class="nav-item active"><span>Inbox</span><span>3</span></div>
  <div class="nav-item"><span>Starred</span><span></span></div>
  <div class="nav-item"><span>Sent</span><span></span></div>
  <div class="nav-item"><span>Drafts</span><span>1</span></div>
  <div class="nav-item"><span>Billing</span><span>12</span></div>
  <div class="nav-item"><span>Archive</span><span></span></div>
</div>
<div class="mail-main">
  <div class="mail-top">
    <div class="mail-search">&#x1F50D;&nbsp;&nbsp;Search mail</div>
  </div>
  <div class="mail-content">
    <div class="mail-subject">Acme invoice 4471 <span class="tag-inbox">Inbox</span></div>
    <div class="sender-row">
      <div class="sender-av">AB</div>
      <div class="sender-meta">
        <div><span class="sender-name">Acme Billing</span><span class="sender-addr">&lt;billing@acme.example&gt;</span></div>
        <div class="sender-to">to finance@contoso.example</div>
      </div>
      <div class="sender-date">26 Nov 2026, 09:14</div>
    </div>
    <div class="inv-card">
      <div class="inv-head">
        <div>
          <div class="inv-title">Acme Billing &mdash; Invoice 4471</div>
          <div style="color:#6b7280;font-size:12px;margin-top:2px;">Billing period: 1 Nov 2026 &ndash; 30 Nov 2026</div>
        </div>
        <span class="inv-badge">Payment due</span>
      </div>
      <table class="inv-table">
        <thead><tr><th>Description</th><th class="num">Qty</th><th class="num">Unit</th><th class="num">Total</th></tr></thead>
        <tbody>
          <tr><td>Managed compute cluster (EU-Central)</td><td class="num">1</td><td class="num">960.00 EUR</td><td class="num">960.00 EUR</td></tr>
          <tr><td>Encrypted backup storage (2 TB)</td><td class="num">1</td><td class="num">180.00 EUR</td><td class="num">180.00 EUR</td></tr>
          <tr><td>Priority support retainer</td><td class="num">1</td><td class="num">100.00 EUR</td><td class="num">100.00 EUR</td></tr>
        </tbody>
      </table>
      <div class="inv-summary">
        <div>
          <div style="font-size:12px;color:#4b5563;">Due date: <strong>30 November 2026</strong></div>
          <div style="font-size:12px;color:#4b5563;">Reference: ACME-2026-4471</div>
        </div>
        <div class="inv-amount">Amount due: 1,240.00 EUR</div>
      </div>
      <div class="inv-pay">
        <span>Pay by card:</span>
        <span class="pay-pill">[redacted]</span>
        <span style="color:#6b7280;">or wire transfer using reference above.</span>
      </div>
    </div>
    <div class="mail-actions">
      <div class="btn-outline">&#x21A9; Reply</div>
      <div class="btn-outline">&#x21AA; Forward</div>
    </div>
  </div>
</div>
"""
    return _browser_shell("Acme invoice 4471 - Mail", "https://mail.google.com/mail/u/0/#inbox/FMfcgzQX4471", content, css, "#d93025")


# ---------------- 2. Docs: Quarterly planning ----------------

def build_docs_planning() -> str:
    css = """
.docs-wrap { flex: 1; display: flex; flex-direction: column; background: #f0f4f9; }
.docs-head { background: #fff; border-bottom: 1px solid #dde3ea; padding: 8px 18px 6px; }
.docs-title-row { display: flex; align-items: center; gap: 10px; }
.docs-ico { width: 26px; height: 26px; border-radius: 5px; background: #2b6cb0; color: #fff; display: flex; align-items: center; justify-content: center; font-weight: 700; font-size: 13px; }
.docs-name { font-size: 16px; font-weight: 600; color: #1f2937; }
.docs-menus { display: flex; gap: 14px; font-size: 12px; color: #4b5563; margin-top: 2px; }
.docs-toolbar {
  margin: 8px 18px 0; background: #edf2fa; border-radius: 18px; height: 34px;
  display: flex; align-items: center; padding: 0 16px; gap: 14px; font-size: 12.5px; color: #374151;
}
.docs-sep { width: 1px; height: 18px; background: #c7d0dc; }
.docs-workspace { flex: 1; display: flex; overflow: hidden; padding: 14px 18px 0; gap: 18px; }
.docs-outline { width: 200px; padding: 8px 6px; font-size: 12px; color: #4b5563; }
.docs-outline h4 { font-size: 11px; text-transform: uppercase; letter-spacing: 0.05em; color: #6b7280; margin-bottom: 10px; }
.docs-outline div { padding: 5px 8px; border-radius: 6px; }
.docs-outline .act { background: #dbeafe; color: #1d4ed8; font-weight: 600; }
.docs-page {
  flex: 1; max-width: 780px; background: #ffffff; border: 1px solid #d7dde6;
  box-shadow: 0 2px 8px rgba(0,0,0,0.06); padding: 36px 52px; color: #1f2937;
}
.docs-page h1 { font-size: 24px; font-weight: 700; margin-bottom: 6px; color: #111827; }
.docs-meta { font-size: 12.5px; color: #6b7280; margin-bottom: 18px; display: flex; gap: 20px; }
.docs-callout { background: #eff6ff; border-left: 4px solid #2563eb; padding: 10px 14px; border-radius: 0 6px 6px 0; margin-bottom: 18px; font-size: 13.5px; }
.docs-page h2 { font-size: 15px; font-weight: 700; margin: 16px 0 8px; color: #1e3a8a; }
.plan-tbl { width: 100%; border-collapse: collapse; font-size: 12.5px; margin-top: 6px; }
.plan-tbl th { background: #f3f6fb; text-align: left; padding: 7px 10px; border: 1px solid #dbe2ec; color: #374151; }
.plan-tbl td { padding: 7px 10px; border: 1px solid #e5eaf1; }
.pill { display: inline-block; padding: 2px 8px; border-radius: 10px; font-size: 11px; font-weight: 600; }
.pill.green { background: #dcfce7; color: #166534; }
.pill.blue { background: #dbeafe; color: #1e40af; }
.pill.amber { background: #fef3c7; color: #92400e; }
"""
    content = """
<div class="docs-wrap">
  <div class="docs-head">
    <div class="docs-title-row">
      <div class="docs-ico">D</div>
      <div>
        <div class="docs-name">Quarterly planning</div>
        <div class="docs-menus"><span>File</span><span>Edit</span><span>View</span><span>Insert</span><span>Format</span><span>Tools</span></div>
      </div>
      <div style="margin-left:auto;background:#c2e7ff;color:#001d35;padding:6px 16px;border-radius:16px;font-weight:600;font-size:12.5px;">Share</div>
    </div>
  </div>
  <div class="docs-toolbar">
    <span>&#x21A9;</span><span>&#x21AA;</span><span class="docs-sep"></span>
    <span>Normal text &#x25BE;</span><span class="docs-sep"></span>
    <span>Segoe UI &#x25BE;</span><span>11</span><span class="docs-sep"></span>
    <strong>B</strong><em>I</em><u>U</u><span class="docs-sep"></span>
    <span>&#x2261;</span><span>&#x2022; List</span>
  </div>
  <div class="docs-workspace">
    <div class="docs-outline">
      <h4>Outline</h4>
      <div class="act">Quarterly planning</div>
      <div>1. Goals</div>
      <div>2. Milestone schedule</div>
      <div>3. Open questions</div>
    </div>
    <div class="docs-page">
      <h1>Quarterly planning</h1>
      <div class="docs-meta">
        <span><strong>Owner:</strong> design team</span>
        <span><strong>Review on Friday</strong></span>
        <span>Status: Draft v2</span>
      </div>
      <div class="docs-callout">
        <strong>Goals:</strong> ship replay export, settings search and text recognition
      </div>
      <h2>1. Milestone schedule</h2>
      <table class="plan-tbl">
        <thead><tr><th>Workstream</th><th>Owner</th><th>Target</th><th>Status</th></tr></thead>
        <tbody>
          <tr><td>Replay export (.mp4 &amp; encrypted archive)</td><td>Core runtime</td><td>Week 2</td><td><span class="pill green">In review</span></td></tr>
          <tr><td>Settings search &amp; deep links</td><td>Design team</td><td>Week 3</td><td><span class="pill blue">In progress</span></td></tr>
          <tr><td>Text recognition settings &amp; script packs</td><td>ML pipeline</td><td>Week 4</td><td><span class="pill blue">In progress</span></td></tr>
          <tr><td>Unlock flow &amp; high-contrast polish</td><td>UI team</td><td>Week 5</td><td><span class="pill amber">Planned</span></td></tr>
        </tbody>
      </table>
      <h2>2. Review on Friday</h2>
      <p style="font-size:13px;color:#374151;line-height:1.6;">
        Walk through the updated settings search layout, confirm the default retention window for exported clips,
        and sign off on the OCR language selector copy before freeze.
      </p>
    </div>
  </div>
</div>
"""
    return _browser_shell("Quarterly planning - Docs", "https://docs.google.com/document/d/q4-quarterly-planning/edit", content, css, "#2563eb")


# ---------------- 3. Docs: NavigationView class ----------------

def build_docs_navview() -> str:
    css = """
.doc-shell { flex: 1; display: flex; flex-direction: column; background: #ffffff; }
.doc-top {
  height: 46px; border-bottom: 1px solid #e1e4e8; padding: 0 24px;
  display: flex; align-items: center; gap: 22px; background: #fafbfc;
}
.doc-brand { font-weight: 700; font-size: 15px; color: #111827; }
.doc-top-nav { display: flex; gap: 18px; font-size: 12.5px; color: #4b5563; }
.doc-top-search { margin-left: auto; width: 240px; height: 28px; border: 1px solid #d1d5db; border-radius: 4px; background: #fff; padding: 0 10px; display: flex; align-items: center; color: #6b7280; font-size: 12px; }
.doc-cols { flex: 1; display: flex; overflow: hidden; }
.doc-toc { width: 245px; border-right: 1px solid #e5e7eb; padding: 16px 14px; font-size: 12.5px; color: #374151; background: #fcfcfd; }
.toc-h { font-weight: 700; color: #111827; margin-bottom: 8px; }
.toc-i { padding: 5px 10px; border-radius: 4px; margin-bottom: 2px; }
.toc-i.sub { padding-left: 22px; color: #4b5563; }
.toc-i.active { background: #e8f0fe; color: #0b57d0; font-weight: 600; border-left: 3px solid #0b57d0; }
.doc-main { flex: 1; padding: 24px 36px; overflow: hidden; }
.doc-crumb { font-size: 11.5px; color: #6b7280; margin-bottom: 8px; }
.doc-main h1 { font-size: 26px; font-weight: 700; color: #111827; margin-bottom: 6px; }
.doc-ns { font-size: 12px; color: #4b5563; margin-bottom: 14px; }
.doc-code {
  background: #f6f8fa; border: 1px solid #e1e4e8; border-radius: 6px;
  padding: 12px 16px; font-size: 12.5px; color: #1f2937; margin: 12px 0 18px;
}
.kw { color: #0550ae; font-weight: 600; }
.tp { color: #0a7b66; }
.prop-tbl { width: 100%; border-collapse: collapse; font-size: 12.5px; }
.prop-tbl th { text-align: left; padding: 7px 10px; border-bottom: 2px solid #d1d5db; color: #374151; }
.prop-tbl td { padding: 8px 10px; border-bottom: 1px solid #edf0f3; vertical-align: top; }
.prop-name { color: #0b57d0; font-weight: 600; }
.doc-right { width: 190px; border-left: 1px solid #eef0f3; padding: 20px 16px; font-size: 12px; color: #4b5563; }
.doc-right h4 { font-size: 11px; text-transform: uppercase; color: #6b7280; margin-bottom: 10px; }
.doc-right div { padding: 4px 0; }
"""
    content = """
<div class="doc-shell">
  <div class="doc-top">
    <div class="doc-brand">Developer Docs</div>
    <div class="doc-top-nav"><span>Windows Apps</span><span>API Reference</span><span>Design</span><span>Samples</span></div>
    <div class="doc-top-search">Filter by title...</div>
  </div>
  <div class="doc-cols">
    <div class="doc-toc">
      <div class="toc-h">UI.Xaml.Controls</div>
      <div class="toc-i">AppBarButton</div>
      <div class="toc-i">BreadcrumbBar</div>
      <div class="toc-i">ContentDialog</div>
      <div class="toc-i active">NavigationView</div>
      <div class="toc-i sub">Constructors</div>
      <div class="toc-i sub">Properties</div>
      <div class="toc-i sub">Methods</div>
      <div class="toc-i sub">Events</div>
      <div class="toc-i">NavigationViewItem</div>
      <div class="toc-i">TabView</div>
    </div>
    <div class="doc-main">
      <div class="doc-crumb">Docs / Windows / API / UI.Xaml.Controls / NavigationView</div>
      <h1>NavigationView Class</h1>
      <div class="doc-ns">Namespace: <span class="mono">UI.Xaml.Controls</span> &nbsp;&bull;&nbsp; Assembly: <span class="mono">WinUI.Controls.dll</span></div>
      <p style="font-size:13.5px;color:#374151;margin-bottom:12px;">
        Represents a container that enables navigation of app content. It has a header, a view for the main content,
        and a menu pane for navigation commands that adapts across compact, minimal, and expanded window widths.
      </p>
      <div class="doc-code mono">
        <span class="kw">public class</span> <span class="tp">NavigationView</span> : <span class="tp">ContentControl</span>
      </div>
      <h2 style="font-size:16px;margin:14px 0 8px;">Properties</h2>
      <table class="prop-tbl">
        <thead><tr><th style="width:32%">Name</th><th>Description</th></tr></thead>
        <tbody>
          <tr><td class="prop-name mono">PaneDisplayMode</td><td>Gets or sets how and where the navigation pane is shown (Auto, Left, LeftCompact, Top).</td></tr>
          <tr><td class="prop-name mono">MenuItems</td><td>Gets the collection of menu items displayed in the navigation menu.</td></tr>
          <tr><td class="prop-name mono">SelectedItem</td><td>Gets or sets the currently selected item in the navigation pane.</td></tr>
          <tr><td class="prop-name mono">IsBackButtonVisible</td><td>Gets or sets a value that indicates whether the back button is shown.</td></tr>
          <tr><td class="prop-name mono">CompactModeThresholdWidth</td><td>Minimum window width at which the pane switches from Minimal to Compact mode.</td></tr>
        </tbody>
      </table>
    </div>
    <div class="doc-right">
      <h4>In this article</h4>
      <div style="color:#0b57d0;font-weight:600;">Definition</div>
      <div>Examples</div>
      <div>Constructors</div>
      <div>Properties</div>
      <div>Methods</div>
      <div>Events</div>
    </div>
  </div>
</div>
"""
    return _browser_shell("NavigationView class - Docs", "https://learn.microsoft.com/en-us/windows/apps/api/navigationview", content, css, "#0078d4")


# ---------------- 4. Flights: Hanoi to Tokyo ----------------

def build_flights() -> str:
    css = """
.fl-page { flex: 1; background: #f8f9fa; padding: 20px 80px; overflow: hidden; display: flex; flex-direction: column; gap: 16px; }
.fl-head { display: flex; align-items: center; justify-content: space-between; }
.fl-brand { font-size: 20px; font-weight: 700; color: #1f2937; }
.fl-search-card {
  background: #fff; border: 1px solid #dadce0; border-radius: 12px;
  padding: 16px 20px; box-shadow: 0 1px 3px rgba(0,0,0,0.05);
}
.fl-opts { display: flex; gap: 18px; font-size: 12.5px; color: #4b5563; margin-bottom: 12px; }
.fl-inputs { display: grid; grid-template-columns: 1.3fr 1.3fr 1fr 1fr; gap: 10px; }
.fl-box {
  border: 1px solid #cfd4dc; border-radius: 8px; padding: 9px 14px;
  background: #fff; font-size: 13.5px; font-weight: 500; color: #1f2937;
}
.fl-box span { display: block; font-size: 11px; color: #6b7280; font-weight: 400; }
.fl-chips { display: flex; gap: 8px; margin-top: 12px; }
.fl-chip { border: 1px solid #d1d5db; background: #fff; border-radius: 16px; padding: 4px 12px; font-size: 12px; color: #374151; }
.fl-chip.on { background: #e8f0fe; border-color: #8ab4f8; color: #1967d2; font-weight: 600; }
.fl-sec { font-size: 16px; font-weight: 600; color: #1f2937; margin-top: 2px; }
.fl-list { background: #fff; border: 1px solid #dadce0; border-radius: 12px; overflow: hidden; }
.fl-row {
  display: grid; grid-template-columns: 44px 1.6fr 1.1fr 0.9fr 1fr; align-items: center;
  padding: 16px 22px; border-bottom: 1px solid #eef0f3;
}
.fl-row:last-child { border-bottom: none; }
.fl-badge {
  width: 32px; height: 32px; border-radius: 8px; color: #fff; font-weight: 700;
  font-size: 12px; display: flex; align-items: center; justify-content: center;
}
.fl-time { font-size: 15px; font-weight: 600; color: #111827; }
.fl-air { font-size: 12px; color: #5f6368; margin-top: 2px; }
.fl-dur { font-size: 14px; color: #1f2937; }
.fl-route { font-size: 12px; color: #5f6368; }
.fl-price { text-align: right; font-size: 17px; font-weight: 700; color: #137333; }
.fl-sub { text-align: right; font-size: 11.5px; color: #5f6368; }
.fl-note {
  background: #e8f0fe; border: 1px solid #d2e3fc; border-radius: 10px;
  padding: 10px 16px; font-size: 12.5px; color: #174ea6; display: flex; justify-content: space-between;
}
"""
    content = """
<div class="fl-page">
  <div class="fl-head">
    <div class="fl-brand">&#x2708; Flights</div>
    <div style="font-size:12.5px;color:#5f6368;">Currency: USD &nbsp;&bull;&nbsp; Direct flights</div>
  </div>
  <div class="fl-search-card">
    <div class="fl-opts"><span>&#x21C4; Round trip &#x25BE;</span><span>1 passenger &#x25BE;</span><span>Economy &#x25BE;</span></div>
    <div class="fl-inputs">
      <div class="fl-box"><span>From</span>Hanoi (HAN)</div>
      <div class="fl-box"><span>To</span>Tokyo (NRT)</div>
      <div class="fl-box"><span>Departure</span>14 Dec</div>
      <div class="fl-box"><span>Return</span>21 Dec</div>
    </div>
    <div class="fl-chips">
      <div class="fl-chip on">Nonstop</div>
      <div class="fl-chip">Airlines &#x25BE;</div>
      <div class="fl-chip">Bags &#x25BE;</div>
      <div class="fl-chip">Price &#x25BE;</div>
      <div class="fl-chip">Times &#x25BE;</div>
    </div>
  </div>
  <div class="fl-sec">Best departing flights &mdash; Hanoi (HAN) to Tokyo (NRT), Departure 14 Dec</div>
  <div class="fl-list">
    <div class="fl-row">
      <div class="fl-badge" style="background:#00688b;">VN</div>
      <div><div class="fl-time">08:15 &ndash; 15:20</div><div class="fl-air">Vietnam Airlines &bull; VN 310</div></div>
      <div><div class="fl-dur">6 h 05 min</div><div class="fl-route">HAN&ndash;NRT</div></div>
      <div class="fl-dur">Nonstop</div>
      <div><div class="fl-price">412 USD</div><div class="fl-sub">round trip</div></div>
    </div>
    <div class="fl-row">
      <div class="fl-badge" style="background:#b91c1c;">JL</div>
      <div><div class="fl-time">11:20 &ndash; 18:10</div><div class="fl-air">Japan Airlines &bull; JL 752</div></div>
      <div><div class="fl-dur">5 h 50 min</div><div class="fl-route">HAN&ndash;NRT</div></div>
      <div class="fl-dur">Nonstop</div>
      <div><div class="fl-price" style="color:#1f2937;">455 USD</div><div class="fl-sub">round trip</div></div>
    </div>
    <div class="fl-row">
      <div class="fl-badge" style="background:#1d4ed8;">PA</div>
      <div><div class="fl-time">23:40 &ndash; 06:55+1</div><div class="fl-air">Pacific Air &bull; PA 804</div></div>
      <div><div class="fl-dur">6 h 15 min</div><div class="fl-route">HAN&ndash;NRT</div></div>
      <div class="fl-dur">Nonstop</div>
      <div><div class="fl-price">389 USD</div><div class="fl-sub">round trip</div></div>
    </div>
  </div>
  <div class="fl-note">
    <span><strong>Price insight:</strong> Prices for 14 Dec are currently typical for Hanoi to Tokyo.</span>
    <span>Track prices &#x2610;</span>
  </div>
</div>
"""
    return _browser_shell("Flights Hanoi to Tokyo - Flights", "https://google.com/travel/flights?q=HAN+to+NRT+14+Dec", content, css, "#1a73e8")


# ---------------- 5. Pull request #12: replay export ----------------

def build_pr_replay() -> str:
    css = """
.gh-wrap { flex: 1; display: flex; flex-direction: column; background: #ffffff; }
.gh-head { background: #f6f8fa; border-bottom: 1px solid #d0d7de; padding: 14px 32px 0; }
.gh-repo { font-size: 13px; color: #57606a; margin-bottom: 6px; }
.gh-repo strong { color: #0969da; }
.gh-title { font-size: 20px; font-weight: 600; color: #1f2328; display: flex; align-items: center; gap: 10px; }
.gh-num { color: #57606a; font-weight: 400; }
.gh-meta { display: flex; align-items: center; gap: 10px; margin: 8px 0 12px; font-size: 12.5px; color: #57606a; }
.gh-open { background: #1f883d; color: #fff; font-weight: 600; padding: 2px 10px; border-radius: 14px; font-size: 12px; }
.gh-branch { background: #ddf4ff; color: #0969da; padding: 1px 7px; border-radius: 6px; font-size: 12px; }
.gh-tabs { display: flex; gap: 18px; font-size: 13px; color: #57606a; }
.gh-tabs span { padding: 8px 4px; border-bottom: 2px solid transparent; }
.gh-tabs .on { color: #1f2328; font-weight: 600; border-bottom-color: #fd8c73; }
.gh-body { flex: 1; padding: 18px 32px; overflow: hidden; display: flex; flex-direction: column; gap: 14px; }
.diff-box { border: 1px solid #d0d7de; border-radius: 6px; overflow: hidden; }
.diff-head {
  background: #f6f8fa; border-bottom: 1px solid #d0d7de; padding: 8px 14px;
  display: flex; align-items: center; justify-content: space-between; font-size: 12.5px;
}
.diff-stat { font-weight: 600; }
.diff-stat .add { color: #1a7f37; }
.diff-stat .del { color: #cf222e; margin-left: 6px; }
.diff-tbl { width: 100%; border-collapse: collapse; font-size: 12px; line-height: 1.55; }
.diff-tbl td { padding: 1px 10px; white-space: pre; }
.ln { width: 42px; text-align: right; color: #6e7781; user-select: none; border-right: 1px solid #eee; }
.hunk { background: #ddf4ff; color: #57606a; }
.ins { background: #dafbe1; color: #116329; }
.rem { background: #ffebe9; color: #82071e; }
.rev-note {
  border: 1px solid #d0d7de; border-radius: 6px; padding: 12px 16px; background: #f6f8fa;
  display: flex; justify-content: space-between; align-items: center; font-size: 12.5px;
}
"""
    content = """
<div class="gh-wrap">
  <div class="gh-head">
    <div class="gh-repo">contoso / <strong>betterrecall</strong></div>
    <div class="gh-title">Pull request #12: replay export <span class="gh-num">#12</span></div>
    <div class="gh-meta">
      <span class="gh-open">Open</span>
      <span>maintainer wants to merge 3 commits into <span class="gh-branch mono">main</span> from <span class="gh-branch mono">feat/replay-export</span></span>
    </div>
    <div class="gh-tabs">
      <span>Conversation 4</span><span>Commits 3</span><span>Checks 4</span><span class="on">Files changed 3</span>
    </div>
  </div>
  <div class="gh-body">
    <div class="diff-box">
      <div class="diff-head">
        <span class="mono">src/Replay/ReplayExporter.cs</span>
        <span class="diff-stat mono"><span class="add">+28</span><span class="del">-6</span></span>
      </div>
      <table class="diff-tbl mono">
        <tr class="hunk"><td class="ln">...</td><td class="ln">...</td><td>@@ -41,14 +41,24 @@ public sealed class ReplayExporter</td></tr>
        <tr><td class="ln">41</td><td class="ln">41</td><td>     public async Task&lt;ExportResult&gt; ExportAsync(string clipId, string targetPath, CancellationToken ct)</td></tr>
        <tr><td class="ln">42</td><td class="ln">42</td><td>     {</td></tr>
        <tr class="rem"><td class="ln">43</td><td class="ln"></td><td>-        var raw = await File.ReadAllBytesAsync(clip.Path, ct);</td></tr>
        <tr class="rem"><td class="ln">44</td><td class="ln"></td><td>-        await File.WriteAllBytesAsync(targetPath, raw, ct);</td></tr>
        <tr class="ins"><td class="ln"></td><td class="ln">43</td><td>+        var tmp = targetPath + ".part";</td></tr>
        <tr class="ins"><td class="ln"></td><td class="ln">44</td><td>+        try</td></tr>
        <tr class="ins"><td class="ln"></td><td class="ln">45</td><td>+        {</td></tr>
        <tr class="ins"><td class="ln"></td><td class="ln">46</td><td>+            await using var src = OpenDecryptedStream(clip);</td></tr>
        <tr class="ins"><td class="ln"></td><td class="ln">47</td><td>+            await using var dst = File.Create(tmp);</td></tr>
        <tr class="ins"><td class="ln"></td><td class="ln">48</td><td>+            await src.CopyToAsync(dst, bufferSize: 128 * 1024, ct);</td></tr>
        <tr class="ins"><td class="ln"></td><td class="ln">49</td><td>+        }</td></tr>
        <tr class="ins"><td class="ln"></td><td class="ln">50</td><td>+        catch { TryDelete(tmp); throw; }</td></tr>
        <tr class="ins"><td class="ln"></td><td class="ln">51</td><td>+        File.Move(tmp, targetPath, overwrite: true);</td></tr>
        <tr><td class="ln">45</td><td class="ln">52</td><td>         _audit.Record("replay export", 1);</td></tr>
        <tr><td class="ln">46</td><td class="ln">53</td><td>         return new ExportResult(clip.Id, new FileInfo(targetPath).Length);</td></tr>
        <tr><td class="ln">47</td><td class="ln">54</td><td>     }</td></tr>
      </table>
    </div>
    <div class="rev-note">
      <div><strong>reviewer</strong> approved these changes &mdash; &ldquo;Clean streaming path, no partial file left on cancel.&rdquo;</div>
      <div style="background:#1f883d;color:#fff;padding:5px 14px;border-radius:6px;font-weight:600;">Merge pull request</div>
    </div>
  </div>
</div>
"""
    return _browser_shell("Pull request #12: replay export", "https://github.com/contoso/betterrecall/pull/12/files", content, css, "#24292f")


# ---------------- 6-8. Code editor (dark) ----------------

def _code_editor_shell(title: str, workspace: str, active_file: str, files: list[tuple[str, int, bool]], tabs: list[str], editor_html: str, status_lang: str, status_pos: str) -> str:
    css = """
body { background: #1e1e1e; color: #d4d4d4; display: flex; flex-direction: column; }
.ce-title {
  height: 32px; background: #2b2b2b; border-bottom: 1px solid #181818;
  display: flex; align-items: center; padding-left: 12px; gap: 14px; font-size: 12px; color: #cccccc;
}
.ce-menus { display: flex; gap: 12px; color: #a9a9a9; }
.ce-mid { margin: 0 auto; color: #d4d4d4; font-weight: 500; }
.ce-main { flex: 1; display: flex; overflow: hidden; }
.ce-act {
  width: 48px; background: #252526; border-right: 1px solid #1b1b1b;
  display: flex; flex-direction: column; align-items: center; padding-top: 12px; gap: 18px;
  color: #858585; font-size: 17px;
}
.ce-act .on { color: #ffffff; border-left: 2px solid #007acc; width: 100%; text-align: center; padding: 4px 0; }
.ce-side { width: 228px; background: #252526; border-right: 1px solid #1b1b1b; padding: 10px 0; font-size: 12.5px; }
.ce-side-h { font-size: 11px; letter-spacing: 0.06em; color: #969696; padding: 0 14px 8px; text-transform: uppercase; }
.ce-tree-item { padding: 4px 12px; color: #cccccc; white-space: nowrap; }
.ce-tree-item.active { background: #37373d; color: #ffffff; }
.ce-editor { flex: 1; display: flex; flex-direction: column; background: #1e1e1e; overflow: hidden; }
.ce-tabs { height: 35px; background: #252526; display: flex; border-bottom: 1px solid #1b1b1b; }
.ce-tab {
  padding: 0 16px; display: flex; align-items: center; gap: 8px; font-size: 12.5px;
  color: #969696; border-right: 1px solid #1b1b1b;
}
.ce-tab.active { background: #1e1e1e; color: #ffffff; border-top: 1px solid #007acc; }
.ce-crumb { height: 24px; padding: 0 16px; display: flex; align-items: center; font-size: 11.5px; color: #8c8c8c; border-bottom: 1px solid #262626; }
.ce-code-area { flex: 1; display: flex; overflow: hidden; }
.ce-code { flex: 1; padding: 12px 16px; font-size: 13px; line-height: 1.58; white-space: pre; overflow: hidden; }
.ce-ln { display: inline-block; width: 34px; color: #6e7681; text-align: right; margin-right: 16px; user-select: none; }
.ce-status {
  height: 24px; background: #007acc; color: #ffffff; font-size: 11.5px;
  display: flex; align-items: center; padding: 0 12px; gap: 16px;
}
.ce-status .r { margin-left: auto; display: flex; gap: 16px; }
.k { color: #569cd6; }
.c { color: #6a9955; }
.s { color: #ce9178; }
.t { color: #4ec9b0; }
.m { color: #dcdcaa; }
.n { color: #b5cea8; }
.p { color: #9cdcfe; }
"""
    tree_html = "".join(
        f'<div class="ce-tree-item{" active" if is_act else ""}" style="padding-left:{14 + depth * 14}px">{name}</div>'
        for name, depth, is_act in files
    )
    tabs_html = "".join(
        f'<div class="ce-tab{" active" if tb == active_file else ""}">{tb}</div>'
        for tb in tabs
    )
    body = f"""
<div class="ce-title">
  <div class="ce-menus"><span>File</span><span>Edit</span><span>Selection</span><span>View</span><span>Go</span><span>Run</span></div>
  <div class="ce-mid">{active_file} &mdash; {workspace} &mdash; Code editor</div>
  {_win_controls(True)}
</div>
<div class="ce-main">
  <div class="ce-act">
    <div class="on">&#x1F4C4;</div>
    <div>&#x1F50D;</div>
    <div>&#x2387;</div>
    <div>&#x25B6;</div>
  </div>
  <div class="ce-side">
    <div class="ce-side-h">Explorer: {workspace}</div>
    {tree_html}
  </div>
  <div class="ce-editor">
    <div class="ce-tabs">{tabs_html}</div>
    <div class="ce-crumb">{workspace} &rsaquo; {active_file}</div>
    <div class="ce-code-area">{editor_html}</div>
  </div>
</div>
<div class="ce-status">
  <span>&#x2387; main</span>
  <span>&#x2297; 0 &nbsp; &#x26A0; 0</span>
  <div class="r"><span>{status_pos}</span><span>Spaces: 4</span><span>UTF-8</span><span>{status_lang}</span></div>
</div>
"""
    return _page(title, css, body)


def build_code_agentclient() -> str:
    lines = [
        '<span class="c">// SPDX-License-Identifier: AGPL-3.0-only</span>',
        '<span class="k">using</span> System.IO.Pipes;',
        '<span class="k">using</span> System.Text;',
        '<span class="k">using</span> BetterRecall.Contracts;',
        '',
        '<span class="k">namespace</span> BetterRecall.UI.Logic;',
        '',
        '<span class="k">public sealed class</span> <span class="t">AgentClient</span> : <span class="t">IAgentClient</span>, <span class="t">IAsyncDisposable</span>',
        '{',
        '    <span class="k">private readonly</span> <span class="k">string</span> _pipeName;',
        '    <span class="k">private readonly</span> <span class="t">IServerTrust</span> _trust;',
        '    <span class="k">private readonly</span> <span class="t">TimeSpan</span> _timeout;',
        '    <span class="k">private</span> <span class="t">NamedPipeClientStream</span>? _pipe;',
        '',
        '    <span class="k">public async</span> <span class="t">Task</span>&lt;<span class="t">AgentResult</span>&gt; <span class="m">SendAsync</span>(<span class="k">string</span> line, <span class="t">CancellationToken</span> ct = <span class="k">default</span>)',
        '    {',
        '        <span class="k">if</span> (Encoding.UTF8.<span class="m">GetByteCount</span>(line) &gt; LineProtocol.MaxUiRequestBytes)',
        '            <span class="k">return</span> AgentResult.<span class="m">Fail</span>(AgentErrorKind.Client, LineProtocol.TooLongUiError);',
        '',
        '        <span class="k">await</span> <span class="m">EnsureConnectedAsync</span>(ct);',
        '        <span class="k">var</span> reply = <span class="k">await</span> <span class="m">WriteAndReadLineAsync</span>(line, ct);',
        '        <span class="k">if</span> (reply <span class="k">is null</span> &amp;&amp; <span class="m">IsReadOnly</span>(line))',
        '        {',
        '            <span class="m">ResetConnection</span>();',
        '            <span class="k">await</span> <span class="m">EnsureConnectedAsync</span>(ct);',
        '            reply = <span class="k">await</span> <span class="m">WriteAndReadLineAsync</span>(line, ct);',
        '        }',
        '        <span class="k">return</span> <span class="m">ParseEnvelope</span>(reply);',
        '    }',
        '}',
    ]
    code_html = '<div class="ce-code mono">' + "\n".join(
        f'<span class="ce-ln">{i + 1}</span>{ln}' for i, ln in enumerate(lines)
    ) + '</div>'
    files = [
        ("&#x25BE; src", 0, False),
        ("&#x25BE; BetterRecall.UI", 1, False),
        ("&#x25BE; Logic", 2, False),
        ("AgentApi.cs", 3, False),
        ("AgentClient.cs", 3, True),
        ("Models.cs", 3, False),
        ("UnlockLogic.cs", 3, False),
        ("&#x25BE; Mock", 3, False),
        ("MockAgent.cs", 4, False),
        ("settings.json", 1, False),
    ]
    return _code_editor_shell(
        "AgentClient.cs - BetterRecall",
        "BetterRecall",
        "AgentClient.cs",
        files,
        ["AgentClient.cs", "AgentApi.cs", "MockAgent.cs"],
        code_html,
        "C#",
        "Ln 21, Col 34",
    )


def build_code_settings() -> str:
    lines = [
        '{',
        '    <span class="p">"capture"</span>: {',
        '        <span class="p">"enabled"</span>: <span class="k">true</span>,',
        '        <span class="p">"intervalMs"</span>: <span class="n">3000</span>,',
        '        <span class="p">"consented"</span>: <span class="k">true</span>',
        '    },',
        '    <span class="p">"audio"</span>: {',
        '        <span class="p">"system"</span>: <span class="k">true</span>,',
        '        <span class="p">"mic"</span>: <span class="k">false</span>',
        '    },',
        '    <span class="p">"retention"</span>: {',
        '        <span class="p">"days"</span>: <span class="n">30</span>,',
        '        <span class="p">"maxStoreGb"</span>: <span class="n">12</span>',
        '    },',
        '    <span class="p">"privacy"</span>: {',
        '        <span class="p">"searchHistory"</span>: <span class="k">true</span>,',
        '        <span class="p">"redactNames"</span>: <span class="k">true</span>,',
        '        <span class="p">"excludedApps"</span>: [<span class="s">"KeePassXC.exe"</span>, <span class="s">"1Password.exe"</span>]',
        '    },',
        '    <span class="p">"security"</span>: {',
        '        <span class="p">"unlockMethod"</span>: <span class="s">"hello"</span>,',
        '        <span class="p">"unlockTimeoutMin"</span>: <span class="n">15</span>,',
        '        <span class="p">"excludeOwnWindows"</span>: <span class="k">true</span>',
        '    },',
        '    <span class="p">"replay"</span>: {',
        '        <span class="p">"enabled"</span>: <span class="k">true</span>,',
        '        <span class="p">"seconds"</span>: <span class="n">120</span>,',
        '        <span class="p">"encryptExports"</span>: <span class="k">true</span>',
        '    }',
        '}',
    ]
    code_html = '<div class="ce-code mono">' + "\n".join(
        f'<span class="ce-ln">{i + 1}</span>{ln}' for i, ln in enumerate(lines)
    ) + '</div>'
    files = [
        ("&#x25BE; BetterRecall", 0, False),
        ("&#x25B8; src", 1, False),
        ("&#x25B8; tests", 1, False),
        ("&#x25B8; config", 1, False),
        ("settings.json", 2, True),
        ("Directory.Build.props", 1, False),
        ("README.md", 1, False),
    ]
    return _code_editor_shell(
        "settings.json - BetterRecall",
        "BetterRecall",
        "settings.json",
        files,
        ["settings.json", "AgentClient.cs"],
        code_html,
        "JSON",
        "Ln 18, Col 24",
    )


def build_code_readme() -> str:
    md_lines = [
        '<span class="k"># Release Notes &mdash; Sprint 14</span>',
        '',
        'Local-first capture, search, and replay export.',
        '',
        '<span class="k">## Highlights</span>',
        '- Streaming replay export to encrypted `.mp4.enc`',
        '- Settings search bar with keyboard focus (`Ctrl+K`)',
        '- Text recognition script selector in Settings',
        '- Timeline paging stability fix',
        '',
        '<span class="k">## Verification Checklist</span>',
        '- [x] Unit tests pass (`dotnet test`)',
        '- [x] Dark &amp; light theme contrast pass',
        '- [x] Lock timeout verified at 15 min',
        '- [ ] Final release tag and changelog',
        '',
        '<span class="k">## Build Command</span>',
        '<span class="s">```powershell</span>',
        'python Scripts/dev_build.py --test --release',
        '<span class="s">```</span>',
    ]
    left = '<div class="ce-code mono" style="border-right:1px solid #2d2d2d;">' + "\n".join(
        f'<span class="ce-ln">{i + 1}</span>{ln}' for i, ln in enumerate(md_lines)
    ) + '</div>'
    right = """
<div style="flex:1;padding:22px 28px;background:#1e1e1e;color:#d4d4d4;overflow:hidden;">
  <h1 style="font-size:22px;color:#fff;border-bottom:1px solid #333;padding-bottom:8px;margin-bottom:12px;">Release Notes &mdash; Sprint 14</h1>
  <p style="color:#b8b8b8;margin-bottom:16px;">Local-first capture, search, and replay export.</p>
  <h2 style="font-size:16px;color:#fff;margin-bottom:8px;">Highlights</h2>
  <ul style="padding-left:20px;color:#cccccc;line-height:1.7;margin-bottom:16px;">
    <li>Streaming replay export to encrypted <code class="mono" style="background:#2d2d2d;padding:1px 5px;border-radius:3px;">.mp4.enc</code></li>
    <li>Settings search bar with keyboard focus (<code class="mono" style="background:#2d2d2d;padding:1px 5px;border-radius:3px;">Ctrl+K</code>)</li>
    <li>Text recognition script selector in Settings</li>
    <li>Timeline paging stability fix</li>
  </ul>
  <h2 style="font-size:16px;color:#fff;margin-bottom:8px;">Verification Checklist</h2>
  <div style="line-height:1.8;color:#cccccc;margin-bottom:16px;">
    <div>&#x2611; Unit tests pass (<code class="mono">dotnet test</code>)</div>
    <div>&#x2611; Dark &amp; light theme contrast pass</div>
    <div>&#x2611; Lock timeout verified at 15 min</div>
    <div>&#x2610; Final release tag and changelog</div>
  </div>
  <h2 style="font-size:16px;color:#fff;margin-bottom:8px;">Build Command</h2>
  <div class="mono" style="background:#252526;border:1px solid #333;border-radius:6px;padding:10px 14px;font-size:12.5px;color:#9cdcfe;">
    python Scripts/dev_build.py --test --release
  </div>
</div>
"""
    files = [
        ("&#x25BE; notes", 0, False),
        ("README.md", 1, True),
        ("sprint-14.md", 1, False),
        ("roadmap.md", 1, False),
        ("decisions.md", 1, False),
    ]
    return _code_editor_shell(
        "README.md - notes",
        "notes",
        "README.md",
        files,
        ["README.md", "Preview README.md"],
        left + right,
        "Markdown",
        "Ln 11, Col 1",
    )


# ---------------- 9-10. Chat app (Teams-like generic) ----------------

def _chat_shell(title: str, active_channel: str, sidebar_items: list[tuple[str, str, bool]], messages_html: str) -> str:
    css = """
body { background: #f5f5f5; color: #242424; display: flex; flex-direction: column; }
.ch-top {
  height: 44px; background: #ebebeb; border-bottom: 1px solid #dcdcdc;
  display: flex; align-items: center; padding-left: 16px; gap: 16px;
}
.ch-brand { font-weight: 700; color: #464eb8; font-size: 14px; }
.ch-search {
  width: 420px; height: 28px; background: #ffffff; border: 1px solid #d4d4d4;
  border-radius: 6px; padding: 0 12px; display: flex; align-items: center; color: #616161; font-size: 12px;
  margin-left: 180px;
}
.ch-main { flex: 1; display: flex; overflow: hidden; }
.ch-rail {
  width: 64px; background: #ebebeb; border-right: 1px solid #dedede;
  display: flex; flex-direction: column; align-items: center; padding-top: 14px; gap: 18px;
  font-size: 11px; color: #616161;
}
.ch-rail .on { color: #464eb8; font-weight: 600; }
.ch-list { width: 256px; background: #f5f5f5; border-right: 1px solid #e0e0e0; padding: 14px 10px; }
.ch-list h3 { font-size: 15px; font-weight: 700; padding: 0 8px 10px; color: #242424; }
.ch-item { padding: 9px 10px; border-radius: 6px; margin-bottom: 3px; }
.ch-item.active { background: #ffffff; box-shadow: 0 1px 2px rgba(0,0,0,0.06); }
.ch-item-t { font-weight: 600; font-size: 13px; color: #242424; }
.ch-item-s { font-size: 11.5px; color: #616161; margin-top: 2px; }
.ch-pane { flex: 1; background: #ffffff; display: flex; flex-direction: column; }
.ch-pane-h {
  height: 52px; border-bottom: 1px solid #e6e6e6; padding: 0 24px;
  display: flex; align-items: center; justify-content: space-between;
}
.ch-pane-title { font-size: 16px; font-weight: 700; color: #242424; }
.ch-msgs { flex: 1; padding: 20px 28px; display: flex; flex-direction: column; gap: 18px; overflow: hidden; }
.msg { display: flex; gap: 12px; }
.msg-av {
  width: 34px; height: 34px; border-radius: 50%; color: #fff; font-weight: 700;
  font-size: 12px; display: flex; align-items: center; justify-content: center; flex-shrink: 0;
}
.msg-body { background: #f5f6fa; border: 1px solid #e6e8f0; border-radius: 8px; padding: 10px 14px; max-width: 720px; }
.msg-head { display: flex; gap: 10px; align-items: baseline; margin-bottom: 4px; }
.msg-name { font-weight: 600; font-size: 13px; color: #1f2328; }
.msg-time { font-size: 11px; color: #6b7280; }
.msg-text { font-size: 13px; color: #242424; line-height: 1.55; }
.ch-compose {
  margin: 12px 28px 18px; border: 1px solid #d1d5db; border-radius: 6px;
  height: 44px; padding: 0 14px; display: flex; align-items: center; justify-content: space-between;
  color: #6b7280; font-size: 13px; background: #fff;
}
"""
    list_html = "".join(
        f'<div class="ch-item{" active" if act else ""}"><div class="ch-item-t">{name}</div><div class="ch-item-s">{sub}</div></div>'
        for name, sub, act in sidebar_items
    )
    body = f"""
<div class="ch-top">
  <div class="ch-brand">&#x1F4AC; Team Chat</div>
  <div class="ch-search">&#x1F50D;&nbsp;&nbsp;Search messages and channels</div>
  {_win_controls(False)}
</div>
<div class="ch-main">
  <div class="ch-rail">
    <div>Activity</div>
    <div class="on">Chat</div>
    <div>Channels</div>
    <div>Calendar</div>
    <div>Files</div>
  </div>
  <div class="ch-list">
    <h3>Channels &amp; Chats</h3>
    {list_html}
  </div>
  <div class="ch-pane">
    <div class="ch-pane-h">
      <div class="ch-pane-title">{active_channel}</div>
      <div style="font-size:12px;color:#616161;">4 members &nbsp;&bull;&nbsp; Files &nbsp;&bull;&nbsp; Notes</div>
    </div>
    <div class="ch-msgs">{messages_html}</div>
    <div class="ch-compose"><span>Type a message...</span><span>&#x27A4;</span></div>
  </div>
</div>
"""
    return _page(title, css, body)


def build_chat_standup() -> str:
    sidebar = [
        ("# Daily standup", "Blockers: none", True),
        ("# General", "Release moved to Friday", False),
        ("# Design review", "Updated card density", False),
        ("# Release 0.4", "CI build #418 passed", False),
        ("# Architecture", "NPU benchmark notes", False),
    ]
    msgs = """
<div class="msg">
  <div class="msg-av" style="background:#4f46e5;">UI</div>
  <div class="msg-body">
    <div class="msg-head"><span class="msg-name">UI Engineer</span><span class="msg-time">09:02</span></div>
    <div class="msg-text">
      <strong>Daily standup</strong><br>
      Yesterday: fixed the timeline paging test<br>
      Today: text recognition settings page<br>
      Blockers: none
    </div>
  </div>
</div>
<div class="msg">
  <div class="msg-av" style="background:#0f766e;">DL</div>
  <div class="msg-body">
    <div class="msg-head"><span class="msg-name">Dev Lead</span><span class="msg-time">09:05</span></div>
    <div class="msg-text">
      Yesterday: replay export streaming path in PR #12<br>
      Today: benchmark run on NPU and review settings search<br>
      Blockers: none
    </div>
  </div>
</div>
<div class="msg">
  <div class="msg-av" style="background:#b45309;">QA</div>
  <div class="msg-body">
    <div class="msg-head"><span class="msg-name">QA Engineer</span><span class="msg-time">09:08</span></div>
    <div class="msg-text">
      Yesterday: verified lock timeout and unlock flow<br>
      Today: dark/light theme pass across Search and Settings<br>
      Blockers: none
    </div>
  </div>
</div>
"""
    return _chat_shell("Daily standup | Team Chat", "Daily standup", sidebar, msgs)


def build_chat_design_review() -> str:
    sidebar = [
        ("Design review", "Looks much cleaner now", True),
        ("Daily standup", "Blockers: none", False),
        ("Release sync", "Checklist ready for Friday", False),
        ("Build alerts", "main: 0 errors, 248 passed", False),
    ]
    msgs = """
<div class="msg">
  <div class="msg-av" style="background:#7c3aed;">PD</div>
  <div class="msg-body">
    <div class="msg-head"><span class="msg-name">Product Designer</span><span class="msg-time">14:12</span></div>
    <div class="msg-text">
      Shared the updated spacing for the search result cards and detail preview pane:
      <ul style="margin:6px 0 0 18px;">
        <li>Thumbnail aspect locked at 16:9 with 8px radius</li>
        <li>Timestamp and app badge aligned on one baseline</li>
        <li>Keyboard focus ring matches system accent in both themes</li>
      </ul>
    </div>
  </div>
</div>
<div class="msg">
  <div class="msg-av" style="background:#0369a1;">UI</div>
  <div class="msg-body">
    <div class="msg-head"><span class="msg-name">UI Engineer</span><span class="msg-time">14:16</span></div>
    <div class="msg-text">
      Looks much cleaner now. I will wire up the keyboard focus states and check high-contrast mode before Friday's review.
    </div>
  </div>
</div>
<div class="msg">
  <div class="msg-av" style="background:#0f766e;">DL</div>
  <div class="msg-body">
    <div class="msg-head"><span class="msg-name">Dev Lead</span><span class="msg-time">14:19</span></div>
    <div class="msg-text">
      Agreed &mdash; let's keep the timeline scrubber ticks subtle so the active window frame stays the focal point.
    </div>
  </div>
</div>
"""
    return _chat_shell("Chat | Design review | Team Chat", "Design review", sidebar, msgs)


# ---------------- 11-12. File Explorer ----------------

def _explorer_shell(folder_name: str, active_nav: str, body_html: str, status_text: str, extra_css: str = "") -> str:
    css = f"""
body {{ background: #f3f3f3; color: #1f1f1f; display: flex; flex-direction: column; }}
.fe-tabs {{
  height: 36px; background: #e8ecf1; display: flex; align-items: flex-end; padding-left: 10px; gap: 4px;
}}
.fe-tab {{
  height: 30px; min-width: 190px; background: #ffffff; border-radius: 8px 8px 0 0;
  padding: 0 12px; display: flex; align-items: center; gap: 8px; font-size: 12px; font-weight: 500;
}}
.fe-addr-bar {{
  height: 42px; background: #ffffff; border-bottom: 1px solid #e5e5e5;
  display: flex; align-items: center; padding: 0 14px; gap: 10px;
}}
.fe-addr {{
  flex: 1; height: 28px; background: #f5f5f5; border: 1px solid #e0e0e0; border-radius: 6px;
  padding: 0 12px; display: flex; align-items: center; gap: 8px; font-size: 12.5px; color: #333;
}}
.fe-search {{
  width: 240px; height: 28px; background: #f5f5f5; border: 1px solid #e0e0e0; border-radius: 6px;
  padding: 0 12px; display: flex; align-items: center; color: #666; font-size: 12px;
}}
.fe-cmd {{
  height: 40px; background: #f9f9f9; border-bottom: 1px solid #e5e5e5;
  display: flex; align-items: center; padding: 0 16px; gap: 18px; font-size: 12.5px; color: #333;
}}
.fe-main {{ flex: 1; display: flex; overflow: hidden; background: #ffffff; }}
.fe-nav {{ width: 210px; border-right: 1px solid #ececec; padding: 12px 8px; font-size: 12.5px; background: #fafafa; }}
.fe-nav-i {{ padding: 6px 12px; border-radius: 5px; display: flex; align-items: center; gap: 8px; color: #333; margin-bottom: 2px; }}
.fe-nav-i.on {{ background: #e8f0fe; color: #0b57d0; font-weight: 600; }}
.fe-content {{ flex: 1; padding: 14px 20px; overflow: hidden; }}
.fe-status {{
  height: 26px; background: #f3f3f3; border-top: 1px solid #e2e2e2;
  padding: 0 16px; display: flex; align-items: center; font-size: 11.5px; color: #555;
}}
{extra_css}
"""
    navs = [("Home", "&#x1F3E0;"), ("Desktop", "&#x1F5A5;"), ("Documents", "&#x1F4C4;"), ("Downloads", "&#x2B07;"), ("Pictures", "&#x1F5BC;"), ("Music", "&#x266B;"), ("Local Disk (C:)", "&#x1F4BD;")]
    nav_html = "".join(
        f'<div class="fe-nav-i{" on" if name == active_nav else ""}"><span>{ico}</span><span>{name}</span></div>'
        for name, ico in navs
    )
    body = f"""
<div class="fe-tabs">
  <div class="fe-tab"><span>&#x1F4C1;</span><span>{folder_name}</span></div>
  {_win_controls(False)}
</div>
<div class="fe-addr-bar">
  <span style="color:#666;">&#x2190; &nbsp; &#x2192; &nbsp; &#x2191;</span>
  <div class="fe-addr">This PC &rsaquo; {folder_name}</div>
  <div class="fe-search">Search {folder_name}</div>
</div>
<div class="fe-cmd">
  <span>&#x2795; New</span><span>&#x2702; Cut</span><span>&#x2398; Copy</span><span>Rename</span><span>Sort &#x25BE;</span><span>View &#x25BE;</span>
</div>
<div class="fe-main">
  <div class="fe-nav">{nav_html}</div>
  <div class="fe-content">{body_html}</div>
</div>
<div class="fe-status">{status_text}</div>
"""
    return _page(f"{folder_name} - File Explorer", css, body)


def build_explorer_downloads() -> str:
    css = """
.dl-tbl { width: 100%; border-collapse: collapse; font-size: 12.5px; }
.dl-tbl th { text-align: left; padding: 7px 10px; border-bottom: 1px solid #e2e2e2; color: #555; font-weight: 600; }
.dl-tbl td { padding: 8px 10px; border-bottom: 1px solid #f2f2f2; }
.dl-tbl tr.sel { background: #e8f0fe; }
.dl-tbl td.sz, .dl-tbl th.sz { text-align: right; }
"""
    rows = [
        ("&#x1F4C4;", "acme-invoice-4471.pdf", "26/11/2026 09:15", "PDF Document", "412 KB", True),
        ("&#x1F4CA;", "Budget 2026.xlsx", "25/11/2026 17:42", "Spreadsheet", "84 KB", False),
        ("&#x1F4E6;", "replay-export-20261124.zip", "24/11/2026 14:10", "Compressed Archive", "68,420 KB", False),
        ("&#x1F4DD;", "Meeting notes 2026-11.docx", "23/11/2026 11:05", "Word Document", "52 KB", False),
        ("&#x1F4E6;", "thesis-figures-v3.zip", "22/11/2026 19:28", "Compressed Archive", "14,210 KB", False),
        ("&#x2699;", "dotnet-sdk-10.0-win-x64.exe", "20/11/2026 10:02", "Application", "218,540 KB", False),
        ("&#x1F4C4;", "contoso-design-tokens.json", "19/11/2026 16:33", "JSON File", "18 KB", False),
        ("&#x1F5BC;", "architecture-overview.svg", "18/11/2026 09:44", "SVG Vector Graphic", "126 KB", False),
    ]
    trs = "".join(
        f'<tr class="{"sel" if sel else ""}"><td>{ico}&nbsp;&nbsp;{name}</td><td>{dt}</td><td>{tp}</td><td class="sz">{sz}</td></tr>'
        for ico, name, dt, tp, sz, sel in rows
    )
    html = f"""
<table class="dl-tbl">
  <thead><tr><th style="width:42%">Name</th><th style="width:22%">Date modified</th><th style="width:22%">Type</th><th class="sz">Size</th></tr></thead>
  <tbody>{trs}</tbody>
</table>
"""
    return _explorer_shell("Downloads", "Downloads", html, "8 items &nbsp;|&nbsp; 1 item selected &nbsp;|&nbsp; 412 KB", css)


def build_explorer_pictures() -> str:
    css = """
.pic-grid { display: grid; grid-template-columns: repeat(4, 1fr); gap: 18px; padding-top: 6px; }
.pic-card {
  border: 1px solid #e5e7eb; border-radius: 8px; padding: 10px; background: #fff;
  display: flex; flex-direction: column; gap: 8px;
}
.pic-card.sel { border-color: #0b57d0; background: #f0f6ff; }
.pic-thumb { height: 148px; border-radius: 6px; overflow: hidden; display: flex; align-items: center; justify-content: center; }
.pic-name { font-size: 12px; font-weight: 500; color: #1f2937; text-align: center; }
.pic-meta { font-size: 11px; color: #6b7280; text-align: center; }
"""
    cards = [
        ("mountain-ridge.jpg", "3840x2160 &bull; 2.4 MB", '<svg viewBox="0 0 200 130" width="100%" height="100%"><defs><linearGradient id="g1" x1="0" y1="0" x2="0" y2="1"><stop offset="0%" stop-color="#3b82f6"/><stop offset="100%" stop-color="#bfdbfe"/></linearGradient></defs><rect width="200" height="130" fill="url(#g1)"/><circle cx="155" cy="34" r="14" fill="#fef08a"/><polygon points="0,130 62,48 122,130" fill="#1e3a8a"/><polygon points="72,130 138,38 200,130" fill="#1d4ed8"/><polygon points="122,60 138,38 154,60" fill="#eff6ff"/></svg>', True),
        ("sunset-harbor.jpg", "2560x1440 &bull; 1.8 MB", '<svg viewBox="0 0 200 130" width="100%" height="100%"><defs><linearGradient id="g2" x1="0" y1="0" x2="0" y2="1"><stop offset="0%" stop-color="#f97316"/><stop offset="65%" stop-color="#fde047"/><stop offset="100%" stop-color="#0f172a"/></linearGradient></defs><rect width="200" height="130" fill="url(#g2)"/><circle cx="100" cy="76" r="22" fill="#fff7ed"/><rect y="82" width="200" height="48" fill="#1e293b" opacity="0.85"/></svg>', False),
        ("whiteboard-q4.png", "1920x1080 &bull; 640 KB", '<svg viewBox="0 0 200 130" width="100%" height="100%"><rect width="200" height="130" fill="#f8fafc"/><rect x="18" y="22" width="52" height="32" rx="6" fill="#dbeafe" stroke="#2563eb" stroke-width="2"/><rect x="126" y="22" width="56" height="32" rx="6" fill="#dcfce7" stroke="#16a34a" stroke-width="2"/><rect x="72" y="80" width="56" height="32" rx="6" fill="#fef3c7" stroke="#d97706" stroke-width="2"/><line x1="70" y1="38" x2="126" y2="38" stroke="#475569" stroke-width="2"/><line x1="100" y1="38" x2="100" y2="80" stroke="#475569" stroke-width="2"/></svg>', False),
        ("forest-trail.jpg", "4032x3024 &bull; 3.1 MB", '<svg viewBox="0 0 200 130" width="100%" height="100%"><rect width="200" height="130" fill="#ecfdf5"/><polygon points="40,120 65,30 90,120" fill="#15803d"/><polygon points="95,120 128,18 161,120" fill="#166534"/><polygon points="145,120 172,44 198,120" fill="#22c55e"/><rect y="114" width="200" height="16" fill="#14532d"/></svg>', False),
        ("ui-wireframe.png", "1600x900 &bull; 310 KB", '<svg viewBox="0 0 200 130" width="100%" height="100%"><rect width="200" height="130" fill="#1e293b"/><rect x="12" y="14" width="42" height="102" rx="4" fill="#334155"/><rect x="64" y="14" width="124" height="24" rx="4" fill="#334155"/><rect x="64" y="46" width="58" height="70" rx="4" fill="#0ea5e9" opacity="0.35"/><rect x="130" y="46" width="58" height="70" rx="4" fill="#475569"/></svg>', False),
        ("tokyo-route.png", "1920x1080 &bull; 520 KB", '<svg viewBox="0 0 200 130" width="100%" height="100%"><rect width="200" height="130" fill="#e0f2fe"/><path d="M24,96 Q96,24 176,42" fill="none" stroke="#0284c7" stroke-width="3" stroke-dasharray="6,4"/><circle cx="24" cy="96" r="6" fill="#0369a1"/><circle cx="176" cy="42" r="6" fill="#dc2626"/></svg>', False),
        ("desk-setup.jpg", "3024x2016 &bull; 2.1 MB", '<svg viewBox="0 0 200 130" width="100%" height="100%"><rect width="200" height="130" fill="#f1f5f9"/><rect x="46" y="20" width="108" height="64" rx="5" fill="#0f172a"/><rect x="52" y="26" width="96" height="52" fill="#38bdf8" opacity="0.35"/><rect x="92" y="84" width="16" height="14" fill="#64748b"/><rect x="20" y="98" width="160" height="14" rx="3" fill="#cbd5e1"/></svg>', False),
        ("night-skyline.jpg", "3840x2160 &bull; 2.9 MB", '<svg viewBox="0 0 200 130" width="100%" height="100%"><rect width="200" height="130" fill="#0f172a"/><rect x="20" y="54" width="28" height="76" fill="#1e293b"/><rect x="54" y="32" width="34" height="98" fill="#334155"/><rect x="94" y="44" width="30" height="86" fill="#1e293b"/><rect x="132" y="24" width="38" height="106" fill="#334155"/><circle cx="38" cy="24" r="2" fill="#fde047"/><circle cx="114" cy="18" r="2" fill="#fde047"/></svg>', False),
    ]
    cards_html = "".join(
        f'<div class="pic-card{" sel" if sel else ""}"><div class="pic-thumb">{svg}</div><div class="pic-name">{name}</div><div class="pic-meta">{meta}</div></div>'
        for name, meta, svg, sel in cards
    )
    return _explorer_shell("Pictures", "Pictures", f'<div class="pic-grid">{cards_html}</div>', "8 items &nbsp;|&nbsp; 1 item selected &nbsp;|&nbsp; 2.4 MB", css)


# ---------------- 13-14. Word documents ----------------

def _word_shell(filename: str, page_html: str, status_left: str) -> str:
    css = """
body { background: #e6e9ef; color: #1f2328; display: flex; flex-direction: column; }
.wd-title {
  height: 36px; background: #185abd; color: #ffffff;
  display: flex; align-items: center; padding-left: 16px; gap: 14px; font-size: 12.5px;
}
.wd-tabs {
  height: 30px; background: #f3f6fb; border-bottom: 1px solid #d9dfe8;
  display: flex; align-items: center; padding: 0 16px; gap: 18px; font-size: 12px; color: #374151;
}
.wd-tabs .on { color: #185abd; font-weight: 700; border-bottom: 2px solid #185abd; padding: 5px 0; }
.wd-rib {
  height: 44px; background: #ffffff; border-bottom: 1px solid #d9dfe8;
  display: flex; align-items: center; padding: 0 20px; gap: 18px; font-size: 12px; color: #374151;
}
.wd-sep { width: 1px; height: 22px; background: #dde2ea; }
.wd-canvas { flex: 1; display: flex; justify-content: center; padding: 18px 0 0; overflow: hidden; }
.wd-page {
  width: 780px; background: #ffffff; border: 1px solid #d1d6df;
  box-shadow: 0 3px 12px rgba(0,0,0,0.08); padding: 40px 58px;
}
.wd-status {
  height: 24px; background: #f3f3f3; border-top: 1px solid #d9dfe8;
  padding: 0 16px; display: flex; align-items: center; justify-content: space-between;
  font-size: 11.5px; color: #4b5563;
}
"""
    body = f"""
<div class="wd-title">
  <strong>Document</strong>
  <span style="margin:0 auto;font-weight:500;">{filename} &mdash; Saved</span>
  {_win_controls(True)}
</div>
<div class="wd-tabs">
  <span>File</span><span class="on">Home</span><span>Insert</span><span>Layout</span><span>References</span><span>Review</span><span>View</span>
</div>
<div class="wd-rib">
  <span>Segoe UI &#x25BE;</span><span>11 &#x25BE;</span><span class="wd-sep"></span>
  <strong>B</strong><em>I</em><u>U</u><span class="wd-sep"></span>
  <span>&#x2261; Left</span><span>&#x2022; Bullets</span><span class="wd-sep"></span>
  <span style="color:#185abd;font-weight:600;">Heading 1</span><span>Normal</span>
</div>
<div class="wd-canvas"><div class="wd-page">{page_html}</div></div>
<div class="wd-status"><span>{status_left}</span><span>100%</span></div>
"""
    return _page(filename, css, body)


def build_word_thesis() -> str:
    page = """
<div style="font-size:11px;text-transform:uppercase;letter-spacing:0.08em;color:#6b7280;margin-bottom:6px;">Chapter 3</div>
<h1 style="font-size:22px;font-weight:700;color:#111827;margin-bottom:14px;">Local-First Semantic Retrieval on Edge NPUs</h1>
<p style="font-size:13.5px;line-height:1.7;color:#24292f;margin-bottom:14px;text-align:justify;">
  Continuous desktop indexing presents a sharp trade-off between recall accuracy, power draw, and user privacy.
  Offloading screen captures to remote inference clusters introduces unacceptable confidentiality risks for personal
  documents, financial records, and private correspondence. In this chapter we evaluate a fully local hybrid retrieval
  pipeline combining quantized multimodal embeddings with lexical full-text search.
</p>
<h2 style="font-size:15px;font-weight:700;color:#185abd;margin:16px 0 8px;">3.1 Hybrid Index Architecture</h2>
<p style="font-size:13.5px;line-height:1.7;color:#24292f;margin-bottom:14px;text-align:justify;">
  Each captured frame is processed asynchronously by an isolated worker process. Text extracted via on-device OCR
  is tokenized into an encrypted FTS5 index, while dense visual embeddings are compressed to int8 vectors and scored
  via reciprocal rank fusion (RRF) at query time.
</p>
<div style="background:#f6f8fc;border:1px solid #dbe4f0;border-radius:6px;padding:12px 18px;margin:14px 0;font-size:13px;" class="mono">
  score(d, q) = w_lex &middot; BM25(d, q) + w_vec &middot; cos(e_d, e_q)
</div>
<h2 style="font-size:15px;font-weight:700;color:#185abd;margin:16px 0 8px;">3.2 Evaluation Methodology</h2>
<p style="font-size:13.5px;line-height:1.7;color:#24292f;text-align:justify;">
  We measure Recall@1, Mean Reciprocal Rank (MRR), and median query latency across a multilingual corpus of desktop
  captures spanning code editors, spreadsheets, technical documentation, and messaging threads.
</p>
"""
    return _word_shell("Thesis draft v3.docx", page, "Page 14 of 48 &nbsp;|&nbsp; 11,420 words &nbsp;|&nbsp; English")


def build_word_meeting_notes() -> str:
    page = """
<h1 style="font-size:21px;font-weight:700;color:#111827;margin-bottom:4px;">Product Sync &mdash; November 2026</h1>
<div style="font-size:12px;color:#6b7280;margin-bottom:18px;">Date: 24 November 2026 &nbsp;&bull;&nbsp; Teams: Product, Engineering, Design, QA</div>
<h2 style="font-size:14.5px;font-weight:700;color:#185abd;margin:12px 0 8px;">Key Decisions</h2>
<ul style="padding-left:20px;font-size:13.5px;line-height:1.7;color:#24292f;margin-bottom:16px;">
  <li>Ship replay export first with atomic temp-file writes and optional encryption enabled by default.</li>
  <li>Keep settings search local and instant, matching both setting labels and section keywords.</li>
  <li>Default PII redaction stays on for card numbers and sensitive prompts before OCR text is persisted.</li>
</ul>
<h2 style="font-size:14.5px;font-weight:700;color:#185abd;margin:14px 0 8px;">Action Items</h2>
<table style="width:100%;border-collapse:collapse;font-size:12.5px;">
  <thead>
    <tr style="background:#f3f6fb;">
      <th style="text-align:left;padding:7px 10px;border:1px solid #d9dfe8;">Owner</th>
      <th style="text-align:left;padding:7px 10px;border:1px solid #d9dfe8;">Task</th>
      <th style="text-align:left;padding:7px 10px;border:1px solid #d9dfe8;">Due</th>
    </tr>
  </thead>
  <tbody>
    <tr><td style="padding:7px 10px;border:1px solid #e5e9f0;">Core runtime</td><td style="padding:7px 10px;border:1px solid #e5e9f0;">Merge PR #12 (replay export) after review</td><td style="padding:7px 10px;border:1px solid #e5e9f0;">27 Nov</td></tr>
    <tr><td style="padding:7px 10px;border:1px solid #e5e9f0;">Design team</td><td style="padding:7px 10px;border:1px solid #e5e9f0;">Finalize Friday quarterly planning deck</td><td style="padding:7px 10px;border:1px solid #e5e9f0;">28 Nov</td></tr>
    <tr><td style="padding:7px 10px;border:1px solid #e5e9f0;">UI team</td><td style="padding:7px 10px;border:1px solid #e5e9f0;">Complete text recognition settings page</td><td style="padding:7px 10px;border:1px solid #e5e9f0;">29 Nov</td></tr>
    <tr><td style="padding:7px 10px;border:1px solid #e5e9f0;">QA</td><td style="padding:7px 10px;border:1px solid #e5e9f0;">Run full light/dark theme verification pass</td><td style="padding:7px 10px;border:1px solid #e5e9f0;">30 Nov</td></tr>
  </tbody>
</table>
"""
    return _word_shell("Meeting notes 2026-11.docx", page, "Page 1 of 2 &nbsp;|&nbsp; 342 words &nbsp;|&nbsp; English")


# ---------------- 15. Spreadsheet: Budget 2026 ----------------

def build_excel_budget() -> str:
    css = """
body { background: #ffffff; color: #1f2328; display: flex; flex-direction: column; }
.xl-title {
  height: 36px; background: #107c41; color: #ffffff;
  display: flex; align-items: center; padding-left: 16px; gap: 14px; font-size: 12.5px;
}
.xl-tabs {
  height: 30px; background: #f3f6f4; border-bottom: 1px solid #d9e2dc;
  display: flex; align-items: center; padding: 0 16px; gap: 18px; font-size: 12px; color: #374151;
}
.xl-tabs .on { color: #107c41; font-weight: 700; border-bottom: 2px solid #107c41; padding: 5px 0; }
.xl-fx {
  height: 32px; background: #ffffff; border-bottom: 1px solid #d9dfe8;
  display: flex; align-items: center; padding: 0 10px; gap: 10px; font-size: 12.5px;
}
.xl-cellref { width: 64px; border-right: 1px solid #dde2ea; text-align: center; font-weight: 600; color: #374151; }
.xl-grid { flex: 1; overflow: hidden; }
.xl-tbl { width: 100%; border-collapse: collapse; table-layout: fixed; font-size: 13px; }
.xl-tbl th {
  background: #f3f4f6; border: 1px solid #d4d8df; height: 26px;
  font-weight: 600; color: #4b5563; font-size: 12px;
}
.xl-tbl td { border: 1px solid #e2e5ea; height: 32px; padding: 0 10px; }
.xl-rn { width: 42px; background: #f3f4f6; text-align: center; color: #6b7280; font-size: 11.5px; }
.num { text-align: right; font-variant-numeric: tabular-nums; }
.hdr td { background: #e6f4ea; font-weight: 700; color: #0d532b; }
.tot td { background: #f3f6f4; font-weight: 700; border-top: 2px solid #107c41; }
.sel { outline: 2px solid #107c41; outline-offset: -2px; background: #f0fdf4; }
.xl-sheets {
  height: 30px; background: #f3f4f6; border-top: 1px solid #d4d8df;
  display: flex; align-items: center; padding: 0 16px; gap: 6px; font-size: 12px;
}
.xl-sheet { padding: 4px 14px; border-radius: 4px 4px 0 0; color: #4b5563; }
.xl-sheet.on { background: #fff; color: #107c41; font-weight: 700; border-bottom: 2px solid #107c41; }
"""
    rows = [
        ("Budget 2026", "", "", "", "", "", "font-weight:700;font-size:15px;color:#107c41;"),
        ("Category", "Q1", "Q2", "Q3", "Q4", "Total", "hdr"),
        ("Hosting", "1200", "1200", "1300", "1300", "5000", ""),
        ("Travel", "800", "400", "950", "600", "2750", ""),
        ("Software licenses", "650", "650", "700", "700", "2700", ""),
        ("Hardware & test rigs", "1500", "300", "450", "900", "3150", ""),
        ("Design & research", "500", "500", "600", "500", "2100", ""),
        ("Contingency", "300", "300", "300", "300", "1200", ""),
        ("Total", "4950", "3350", "4300", "4300", "16900", "tot"),
    ]
    tr_html = []
    for idx, (c1, c2, c3, c4, c5, c6, style) in enumerate(rows, start=1):
        cls = f' class="{style}"' if style in ("hdr", "tot") else ""
        td1_style = f' style="{style}"' if style and style not in ("hdr", "tot") else ""
        sel_cls = " sel" if idx == 9 else ""
        tr_html.append(
            f'<tr{cls}><td class="xl-rn">{idx}</td><td{td1_style}>{c1}</td>'
            f'<td class="num">{c2}</td><td class="num">{c3}</td><td class="num">{c4}</td><td class="num">{c5}</td>'
            f'<td class="num{sel_cls}">{c6}</td><td></td></tr>'
        )
    for idx in range(len(rows) + 1, 18):
        tr_html.append(f'<tr><td class="xl-rn">{idx}</td><td></td><td></td><td></td><td></td><td></td><td></td><td></td></tr>')

    body = f"""
<div class="xl-title">
  <strong>Spreadsheet</strong>
  <span style="margin:0 auto;font-weight:500;">Budget 2026.xlsx &mdash; Saved</span>
  {_win_controls(True)}
</div>
<div class="xl-tabs">
  <span>File</span><span class="on">Home</span><span>Insert</span><span>Formulas</span><span>Data</span><span>View</span>
</div>
<div class="xl-fx">
  <div class="xl-cellref">F9</div>
  <div style="color:#6b7280;font-style:italic;">fx</div>
  <div class="mono">=SUM(B9:E9)</div>
</div>
<div class="xl-grid">
  <table class="xl-tbl">
    <thead>
      <tr><th style="width:42px;"></th><th style="width:240px;">A</th><th style="width:120px;">B</th><th style="width:120px;">C</th><th style="width:120px;">D</th><th style="width:120px;">E</th><th style="width:130px;">F</th><th>G</th></tr>
    </thead>
    <tbody>{"".join(tr_html)}</tbody>
  </table>
</div>
<div class="xl-sheets">
  <div class="xl-sheet on">Summary</div>
  <div class="xl-sheet">Q1-Q2 Detail</div>
  <div class="xl-sheet">Forecast</div>
  <div style="margin-left:auto;color:#555;">Sum: 16,900 &nbsp;|&nbsp; Currency: USD</div>
</div>
"""
    return _page("Budget 2026.xlsx - Spreadsheet", css, body)


# ---------------- 16-17. Terminal windows ----------------

def _terminal_shell(title: str, active_tab: str, other_tab: str, body_html: str) -> str:
    css = """
body { background: #0c0c0c; color: #e5e5e5; display: flex; flex-direction: column; }
.wt-tabs {
  height: 36px; background: #1f1f1f; display: flex; align-items: flex-end; padding-left: 8px; gap: 4px;
}
.wt-tab {
  height: 30px; min-width: 200px; background: #0c0c0c; color: #ffffff;
  border-radius: 6px 6px 0 0; padding: 0 12px; display: flex; align-items: center; gap: 8px; font-size: 12px;
}
.wt-tab.off { background: transparent; color: #9a9a9a; min-width: 170px; }
.wt-body {
  flex: 1; padding: 18px 22px; font-size: 13.5px; line-height: 1.58; white-space: pre; overflow: hidden;
}
.g { color: #4ec9b0; }
.ok { color: #6ccb5f; font-weight: 600; }
.y { color: #f9e2af; }
.b { color: #61afef; }
.dim { color: #8b949e; }
.cyn { color: #56b6c2; }
.mag { color: #c678dd; }
.cur { display: inline-block; width: 8px; height: 15px; background: #e5e5e5; vertical-align: -2px; }
"""
    body = f"""
<div class="wt-tabs">
  <div class="wt-tab"><span>&gt;_</span><span>{active_tab}</span><span style="margin-left:auto;font-size:10px;opacity:0.6;">&#x2715;</span></div>
  <div class="wt-tab off"><span>&gt;_</span><span>{other_tab}</span></div>
  {_win_controls(True)}
</div>
<div class="wt-body mono">{body_html}</div>
"""
    return _page(title, css, body)


def build_term_dotnet() -> str:
    body = """<span class="b">PS E:\\Projects\\BetterRecall&gt;</span> dotnet build -c Release
<span class="dim">MSBuild version 17.14.0+1a2b3c for .NET</span>
  Determining projects to restore...
  All projects are up-to-date for restore.
  BetterRecall.Contracts -&gt; E:\\Projects\\BetterRecall\\artifacts\\bin\\BetterRecall.Contracts\\release\\BetterRecall.Contracts.dll
  BetterRecall.UI -&gt; E:\\Projects\\BetterRecall\\artifacts\\bin\\BetterRecall.UI\\release_win-x64\\BetterRecall.UI.dll
  BetterRecall.UI.Tests -&gt; E:\\Projects\\BetterRecall\\artifacts\\bin\\BetterRecall.UI.Tests\\release\\BetterRecall.UI.Tests.dll

<span class="ok">Build succeeded.</span>
    <span class="ok">0 Warning(s)</span>
    <span class="ok">0 Error(s)</span>

Time Elapsed 00:00:04.82

<span class="b">PS E:\\Projects\\BetterRecall&gt;</span> dotnet test tests\\BetterRecall.UI.Tests --no-build -c Release
  Determining projects to restore...
  Test run for BetterRecall.UI.Tests.dll (.NET 10.0)
  Starting test execution, please wait...
  A total of 1 test files matched the specified pattern.

<span class="ok">Passed!</span>  - Failed:     0, Passed:   142, Skipped:     0, Total:   142, Duration: 2 s

<span class="b">PS E:\\Projects\\BetterRecall&gt;</span> <span class="cur"></span>"""
    return _terminal_shell("dotnet build - Terminal", "dotnet build", "git log", body)


def build_term_gitlog() -> str:
    body = """<span class="b">PS E:\\Projects\\BetterRecall&gt;</span> git log --oneline --graph --decorate -n 14
<span class="mag">*</span> <span class="y">e4a91c2</span> (<span class="cyn">HEAD -&gt; main</span>, <span class="ok">origin/main</span>) feat(ui): embed rendered demo pages for mock captures
<span class="mag">*</span> <span class="y">b81d04f</span> feat(replay): stream decrypted export via atomic temp file (#12)
<span class="mag">*</span> <span class="y">9c3e71a</span> fix(timeline): stabilize paging boundary on minute rollover
<span class="mag">*</span> <span class="y">72f4a80</span> feat(settings): add instant search bar and keyboard focus
<span class="mag">*</span> <span class="y">5d0b619</span> feat(ocr): add script selector for multilingual recognition
<span class="mag">*</span> <span class="y">3a91e44</span> (<span class="y">tag: v0.3.2</span>) chore(release): bump version to 0.3.2
<span class="mag">*</span> <span class="y">1f6c82d</span> fix(unlock): preserve password retry countdown across reconnect
<span class="mag">*</span> <span class="y">c84a205</span> perf(search): cache decoded thumbnails per width in UI viewmodel
<span class="mag">*</span> <span class="y">a19f53b</span> docs: refresh dark and light screenshots for README
<span class="mag">*</span> <span class="y">84e2c11</span> test(ui): add one-shot search non-blocking pipe regression test
<span class="mag">*</span> <span class="y">6b7d904</span> feat(privacy): redact card numbers before storing OCR text
<span class="mag">*</span> <span class="y">491c3e8</span> refactor(ipc): unify error envelopes for locked and paused states
<span class="mag">*</span> <span class="y">20f8a77</span> feat(chat): show approval thumbnails before sharing captures
<span class="mag">*</span> <span class="y">0d54b12</span> chore(ci): pin SDK and enable deterministic release builds

<span class="b">PS E:\\Projects\\BetterRecall&gt;</span> <span class="cur"></span>"""
    return _terminal_shell("git log - Terminal", "git log", "dotnet build", body)


# ---------------- 18. Music player ----------------

def build_music_player() -> str:
    css = """
body { background: #121212; color: #ffffff; display: flex; flex-direction: column; }
.mp-top {
  height: 36px; background: #0a0a0a; display: flex; align-items: center; padding-left: 14px; font-size: 12px; color: #b3b3b3;
}
.mp-main { flex: 1; display: flex; overflow: hidden; }
.mp-side {
  width: 230px; background: #000000; padding: 14px 16px; display: flex; flex-direction: column; gap: 10px;
}
.mp-brand { font-size: 16px; font-weight: 700; color: #1ed760; margin-bottom: 6px; }
.mp-nav { font-size: 13.5px; color: #b3b3b3; padding: 4px 0; font-weight: 600; }
.mp-nav.on { color: #ffffff; }
.mp-div { height: 1px; background: #242424; margin: 6px 0; }
.mp-pl { font-size: 12.5px; color: #a7a7a7; padding: 3px 0; }
.mp-content {
  flex: 1; background: linear-gradient(180deg, #1f332c 0%, #121212 38%);
  padding: 22px 28px; overflow: hidden;
}
.mp-h1 { font-size: 26px; font-weight: 700; margin-bottom: 16px; }
.mp-quick { display: grid; grid-template-columns: repeat(3, 1fr); gap: 12px; margin-bottom: 24px; }
.mp-qcard {
  height: 54px; background: rgba(255,255,255,0.08); border-radius: 6px;
  display: flex; align-items: center; gap: 12px; overflow: hidden; font-weight: 600; font-size: 13px;
}
.mp-qart { width: 54px; height: 54px; flex-shrink: 0; }
.mp-h2 { font-size: 18px; font-weight: 700; margin-bottom: 14px; }
.mp-grid { display: grid; grid-template-columns: repeat(5, 1fr); gap: 16px; }
.mp-card { background: #181818; border-radius: 8px; padding: 12px; }
.mp-cover { height: 148px; border-radius: 6px; margin-bottom: 10px; display: flex; align-items: flex-end; padding: 10px; font-weight: 700; font-size: 13px; }
.mp-ctitle { font-weight: 600; font-size: 13.5px; margin-bottom: 3px; }
.mp-csub { font-size: 12px; color: #a7a7a7; }
.mp-bar {
  height: 74px; background: #181818; border-top: 1px solid #282828;
  display: flex; align-items: center; padding: 0 20px; justify-content: space-between;
}
.mp-now { display: flex; align-items: center; gap: 12px; width: 280px; }
.mp-now-art { width: 48px; height: 48px; border-radius: 4px; background: linear-gradient(135deg, #0ea5e9, #1e3a8a); }
.mp-ctrl { display: flex; flex-direction: column; align-items: center; gap: 6px; width: 420px; }
.mp-btns { display: flex; align-items: center; gap: 22px; font-size: 16px; color: #d4d4d4; }
.mp-play { width: 32px; height: 32px; border-radius: 50%; background: #ffffff; color: #000; display: flex; align-items: center; justify-content: center; font-size: 13px; }
.mp-prog { width: 100%; display: flex; align-items: center; gap: 10px; font-size: 11px; color: #a7a7a7; }
.mp-track { flex: 1; height: 4px; background: #4d4d4d; border-radius: 2px; overflow: hidden; }
.mp-fill { width: 44%; height: 100%; background: #1ed760; }
.mp-vol { width: 180px; display: flex; align-items: center; justify-content: flex-end; gap: 10px; color: #b3b3b3; }
"""
    quick = [
        ("Deep Focus", "linear-gradient(135deg,#0f766e,#14b8a6)"),
        ("Lo-Fi Coding", "linear-gradient(135deg,#4338ca,#818cf8)"),
        ("Synthwave Drive", "linear-gradient(135deg,#be185d,#f97316)"),
        ("Acoustic Morning", "linear-gradient(135deg,#b45309,#fbbf24)"),
        ("Late Night Ambient", "linear-gradient(135deg,#1e293b,#38bdf8)"),
        ("Piano Instrumentals", "linear-gradient(135deg,#3f6212,#84cc16)"),
    ]
    albums = [
        ("Northern Drift", "Glass Harbor", "linear-gradient(135deg,#0ea5e9,#1e3a8a)"),
        ("Static Bloom", "Velour", "linear-gradient(135deg,#9333ea,#ec4899)"),
        ("Night Tram", "Kestrel", "linear-gradient(135deg,#f59e0b,#b91c1c)"),
        ("Paper Lanterns", "Sora Ensemble", "linear-gradient(135deg,#10b981,#065f46)"),
        ("Cobalt Tape", "Signal North", "linear-gradient(135deg,#3b82f6,#111827)"),
    ]
    q_html = "".join(
        f'<div class="mp-qcard"><div class="mp-qart" style="background:{bg};"></div><span>{name}</span></div>'
        for name, bg in quick
    )
    a_html = "".join(
        f'<div class="mp-card"><div class="mp-cover" style="background:{bg};">{title}</div><div class="mp-ctitle">{title}</div><div class="mp-csub">{artist}</div></div>'
        for title, artist, bg in albums
    )
    body = f"""
<div class="mp-top">
  <span>Music Player</span>
  {_win_controls(True)}
</div>
<div class="mp-main">
  <div class="mp-side">
    <div class="mp-brand">&#x266B; Music</div>
    <div class="mp-nav on">Home</div>
    <div class="mp-nav">Search</div>
    <div class="mp-nav">Your Library</div>
    <div class="mp-div"></div>
    <div class="mp-pl">Deep Focus</div>
    <div class="mp-pl">Lo-Fi Coding</div>
    <div class="mp-pl">Synthwave Drive</div>
    <div class="mp-pl">Acoustic Morning</div>
    <div class="mp-pl">Late Night Ambient</div>
  </div>
  <div class="mp-content">
    <div class="mp-h1">Good evening</div>
    <div class="mp-quick">{q_html}</div>
    <div class="mp-h2">Made for focus</div>
    <div class="mp-grid">{a_html}</div>
  </div>
</div>
<div class="mp-bar">
  <div class="mp-now">
    <div class="mp-now-art"></div>
    <div>
      <div style="font-weight:600;font-size:13px;">Northern Drift</div>
      <div style="font-size:11.5px;color:#a7a7a7;">Glass Harbor</div>
    </div>
  </div>
  <div class="mp-ctrl">
    <div class="mp-btns"><span>&#x23EE;</span><div class="mp-play">&#x23F8;</div><span>&#x23ED;</span></div>
    <div class="mp-prog"><span>1:42</span><div class="mp-track"><div class="mp-fill"></div></div><span>3:58</span></div>
  </div>
  <div class="mp-vol"><span>&#x1F50A;</span><div class="mp-track" style="max-width:96px;"><div class="mp-fill" style="width:70%;"></div></div></div>
</div>
"""
    return _page("Music Player", css, body)


# ---------------- title -> slug -> builder table ----------------

PAGES: dict[str, tuple[str, Callable[[], str]]] = {
    "Acme invoice 4471 - Gmail": ("mail-invoice", build_mail_invoice),
    "Quarterly planning - Google Docs": ("docs-planning", build_docs_planning),
    "NavigationView class - Microsoft Learn": ("docs-navview", build_docs_navview),
    "Flights Hanoi to Tokyo - Google Flights": ("flights-han-nrt", build_flights),
    "Pull request #12: replay export - GitHub": ("pr-replay-export", build_pr_replay),
    "AgentClient.cs - BetterRecall - Visual Studio Code": ("code-agentclient", build_code_agentclient),
    "settings.json - BetterRecall - Visual Studio Code": ("code-settings", build_code_settings),
    "README.md - notes - Visual Studio Code": ("code-readme", build_code_readme),
    "Daily standup | Microsoft Teams": ("chat-standup", build_chat_standup),
    "Chat | Design review | Microsoft Teams": ("chat-design-review", build_chat_design_review),
    "Downloads - File Explorer": ("explorer-downloads", build_explorer_downloads),
    "Pictures - File Explorer": ("explorer-pictures", build_explorer_pictures),
    "Thesis draft v3.docx - Word": ("word-thesis", build_word_thesis),
    "Meeting notes 2026-11.docx - Word": ("word-meeting-notes", build_word_meeting_notes),
    "Budget 2026.xlsx - Excel": ("excel-budget", build_excel_budget),
    "dotnet build - Windows Terminal": ("term-dotnet-build", build_term_dotnet),
    "git log - Windows Terminal": ("term-git-log", build_term_gitlog),
    "Spotify Premium": ("music-player", build_music_player),
}


def find_edge() -> pathlib.Path:
    for p in EDGE_CANDIDATES:
        if p.is_file():
            return p
    sys.exit("Microsoft Edge (msedge.exe) not found in Program Files.")


def write_html(slug: str, html: str) -> pathlib.Path:
    SRC_DIR.mkdir(parents=True, exist_ok=True)
    out = SRC_DIR / f"{slug}.html"
    out.write_text(html, encoding="utf-8", newline="\n")
    return out


def render(slug: str) -> pathlib.Path:
    from PIL import Image

    edge = find_edge()
    html_path = SRC_DIR / f"{slug}.html"
    if not html_path.is_file():
        sys.exit(f"HTML source missing: {html_path}")
    out_png = DEMO_DIR / f"{slug}.png"
    if out_png.exists():
        out_png.unlink()

    scratch_root = pathlib.Path(r"K:\scratch")
    tmp_parent = str(scratch_root) if scratch_root.is_dir() else None
    profile_dir = tempfile.mkdtemp(prefix=f"br-demo-{slug}-", dir=tmp_parent)
    try:
        cmd = [
            str(edge),
            "--headless=new",
            "--disable-gpu",
            "--hide-scrollbars",
            "--window-size=1280,720",
            f"--user-data-dir={profile_dir}",
            f"--screenshot={out_png}",
            html_path.resolve().as_uri(),
        ]
        subprocess.run(cmd, check=False, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=30)

        deadline = time.monotonic() + 20.0
        last_size = -1
        while time.monotonic() < deadline:
            if out_png.is_file():
                sz = out_png.stat().st_size
                if sz > 0 and sz == last_size:
                    break
                last_size = sz
            time.sleep(0.2)
        if not out_png.is_file() or out_png.stat().st_size == 0:
            sys.exit(f"Timed out waiting for screenshot: {out_png}")

        with Image.open(out_png) as img:
            resized = img.convert("RGB").resize((960, 540), Image.LANCZOS)
            resized.save(out_png, format="PNG", optimize=True)
        return out_png
    finally:
        shutil.rmtree(profile_dir, ignore_errors=True)


def write_index() -> pathlib.Path:
    index = {title: f"{slug}.png" for title, (slug, _) in PAGES.items() if (DEMO_DIR / f"{slug}.png").is_file()}
    INDEX_PATH.write_text(json.dumps(index, indent=2) + "\n", encoding="utf-8", newline="\n")
    return INDEX_PATH


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--only", help="Comma-separated list of slugs to process")
    ap.add_argument("--html-only", action="store_true", help="Write HTML files only without launching Edge")
    args = ap.parse_args()

    slug_to_entry = {slug: (title, builder) for title, (slug, builder) in PAGES.items()}
    if args.only:
        selected = [s.strip() for s in args.only.split(",") if s.strip()]
        unknown = [s for s in selected if s not in slug_to_entry]
        if unknown:
            sys.exit(f"Unknown slug(s): {', '.join(unknown)}")
    else:
        selected = [slug for _, (slug, _) in PAGES.items()]

    for slug in selected:
        _, builder = slug_to_entry[slug]
        html_path = write_html(slug, builder())
        print(f"html {html_path.relative_to(ROOT)}")

    if not args.html_only:
        for slug in selected:
            png_path = render(slug)
            print(f"png  {png_path.relative_to(ROOT)}")
        idx_path = write_index()
        print(f"idx  {idx_path.relative_to(ROOT)}")


if __name__ == "__main__":
    main()
