#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""把下载目录里的封面 / 歌词旁路文件迁移到 image/ 与 lrc/ 子目录。

背景：sodam 0.3.x 之前把封面 / 歌词和音频平铺在下载目录里；之后改为固定落在
<下载根>/image/ 与 <下载根>/lrc/（见仓库 MODIFICATIONS.md「下载旁路改放
image/ 与 lrc/ 子目录」一节）。本脚本把旧布局的存量文件收进新布局，
让 sodam（以及默认就去 image/、lrc/ 找图找词的音乐库软件）能直接找到。

规则（与 sodam `scan_sidecars` 的判定一致）：
  * png / jpg / jpeg / webp  ->  <root>/image/
  * lrc                      ->  <root>/lrc/
  * 只迁移"有同名音频"的文件；没有配对音频的图片 / 歌词默认不动
    （--include-unpaired 可一并迁入）
  * 递归子目录；根下的 image/ 与 lrc/ 会跳过 —— 幂等，可重复运行

冲突处理：
  * 目标已有内容相同的文件  -> 删掉来源副本（去重）
  * 目标已有内容不同的文件  -> 来源改名为 "<stem> (2).<ext>" 后移入

用法：
  python migrate-sidecars.py --root "D:/soft/app/SodaM/down"            # 演练
  python migrate-sidecars.py --root "D:/soft/app/SodaM/down" --apply    # 执行

日志：默认写到 <root>/migrate-sidecars.log（UTF-8），含每一条移动记录，
可据此人工回滚。
"""

from __future__ import annotations

import argparse
import hashlib
import os
import sys
from datetime import datetime
from pathlib import Path

# 与 sodam-core 的常量保持一致（`crates/sodam-core/src/download.rs`）。
IMAGE_EXTS = {".png", ".jpg", ".jpeg", ".webp"}
AUDIO_EXTS = {".m4a", ".mp4", ".flac", ".mp3", ".wav", ".aac"}
TARGET_DIRS = ("image", "lrc")  # 目标子目录名（小写，与 sodam 常量一致）


def category(path: Path) -> str | None:
    """旁路分类：`image` / `lrc`；不是旁路返回 None。"""
    ext = path.suffix.lower()
    if ext in IMAGE_EXTS:
        return "image"
    if ext == ".lrc":
        return "lrc"
    return None


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def same_content(a: Path, b: Path) -> bool:
    try:
        if a.stat().st_size != b.stat().st_size:
            return False
        return sha256(a) == sha256(b)
    except OSError:
        return False


def walk(root: Path) -> tuple[list[Path], set[str]]:
    """返回 (旁路文件, 音频文件名 stem 集合)；跳过根下的 image/ 与 lrc/。"""
    sidecars: list[Path] = []
    audio_stems: set[str] = set()
    for dirpath, dirnames, filenames in os.walk(root, followlinks=False):
        current = Path(dirpath)
        if current == root:
            dirnames[:] = [d for d in dirnames if d.lower() not in TARGET_DIRS]
        for name in filenames:
            path = current / name
            if path.is_symlink() or not path.is_file():
                continue
            ext = path.suffix.lower()
            if ext in AUDIO_EXTS:
                audio_stems.add(path.stem)
            if category(path) is not None:
                sidecars.append(path)
    return sorted(sidecars), audio_stems


def build_plan(
    root: Path,
    sidecars: list[Path],
    audio_stems: set[str],
    include_unpaired: bool,
) -> list[tuple[Path, Path, str]]:
    """生成 (来源, 目标, 动作) 列表。

    动作：
      * `move`     普通移动
      * `rename`   目标名已被占（内容不同）-> 加序号后移入
      * `dedupe`   目标已有内容相同的文件 -> 删来源
      * `placed`   已在正确子目录里（跳过）
      * `unpaired` 没有配对音频（跳过；仅记录）
    """
    # 目标目录里已存在的名字 -> 对应文件（磁盘上的，或本批次里计划移入的来源）。
    taken: dict[str, dict[str, Path]] = {"image": {}, "lrc": {}}
    for target, names in taken.items():
        target_dir = root / target
        if target_dir.is_dir():
            for entry in os.listdir(target_dir):
                names[entry] = target_dir / entry

    plans: list[tuple[Path, Path, str]] = []
    for src in sidecars:
        kind = category(src)
        assert kind is not None  # walk 只收集旁路
        dst_dir = root / kind

        if src.parent == dst_dir:
            plans.append((src, src, "placed"))
            continue
        if not include_unpaired and src.stem not in audio_stems:
            plans.append((src, dst_dir / src.name, "unpaired"))
            continue

        existing = taken[kind].get(src.name)
        if existing is not None:
            if same_content(src, existing):
                plans.append((src, dst_dir / src.name, "dedupe"))
                continue
            index = 2
            while True:
                candidate = f"{src.stem} ({index}){src.suffix}"
                if candidate not in taken[kind]:
                    taken[kind][candidate] = src
                    plans.append((src, dst_dir / candidate, "rename"))
                    break
                index += 1
            continue

        taken[kind][src.name] = src
        plans.append((src, dst_dir / src.name, "move"))
    return plans


def relative(path: Path, root: Path) -> str:
    try:
        return path.relative_to(root).as_posix()
    except ValueError:
        return path.as_posix()


def main() -> int:
    parser = argparse.ArgumentParser(
        description="迁移下载目录里的封面 / 歌词旁路到 image/ 与 lrc/ 子目录（sodam 新布局）",
    )
    parser.add_argument("--root", required=True, help="下载根目录（旧布局）")
    parser.add_argument(
        "--apply",
        action="store_true",
        help="真正执行移动（默认只演练，不动任何文件）",
    )
    parser.add_argument(
        "--include-unpaired",
        action="store_true",
        help="连没有配对音频的图片 / 歌词也一并移入",
    )
    parser.add_argument(
        "--log",
        default=None,
        help="日志文件路径（默认 <root>/migrate-sidecars.log）",
    )
    args = parser.parse_args()

    root = Path(args.root).expanduser()
    if not root.is_dir():
        print(f"[error] not a directory: {root}")
        return 2
    root = root.resolve()

    sidecars, audio_stems = walk(root)
    plans = build_plan(root, sidecars, audio_stems, args.include_unpaired)

    counts = {
        "move": 0,
        "rename": 0,
        "dedupe": 0,
        "placed": 0,
        "unpaired": 0,
    }
    failures: list[tuple[Path, str]] = []
    log_lines: list[str] = []

    def log(line: str) -> None:
        log_lines.append(line)

    for src, dst, action in plans:
        src_rel = relative(src, root)
        if action == "dedupe":
            if args.apply:
                try:
                    src.unlink()
                except OSError as err:
                    failures.append((src, str(err)))
                    log(f"FAIL    {src_rel}  ({err})")
                    continue
            counts["dedupe"] += 1
            log(f"DEDUPE  {src_rel}  (target already has identical copy)")
            continue
        if action in ("placed", "unpaired"):
            counts[action] += 1
            reason = "already in place" if action == "placed" else "no matching audio"
            log(f"KEEP    {src_rel}  ({reason})")
            continue
        # move / rename
        if args.apply:
            dst.parent.mkdir(parents=True, exist_ok=True)
            if dst.exists():
                failures.append((src, f"target appeared concurrently: {relative(dst, root)}"))
                log(f"FAIL    {src_rel}  (target appeared: {relative(dst, root)})")
                continue
            try:
                os.rename(src, dst)
            except OSError as err:
                failures.append((src, str(err)))
                log(f"FAIL    {src_rel}  ({err})")
                continue
        counts[action] += 1
        verb = "MOVE" if action == "move" else "RENAME"
        log(f"{verb:<7} {src_rel}  ->  {relative(dst, root)}")

    image_total = sum(1 for p in sidecars if category(p) == "image")
    lrc_total = sum(1 for p in sidecars if category(p) == "lrc")
    paired = sum(1 for p in sidecars if p.stem in audio_stems)

    # 迁移后两处旁路目录应有的 stem 集合 = 已在目录里的 + 本批次计划移入/改名的
    # （不含 unpaired；它们留在原地）。
    def stems_after(kind: str) -> set[str]:
        stems: set[str] = set()
        target_dir = root / kind
        if target_dir.is_dir():
            stems.update(entry.stem for entry in target_dir.iterdir() if entry.is_file())
        for _, dst, action in plans:
            if dst.parent == target_dir and action in ("move", "rename"):
                stems.add(dst.stem)
        return stems

    image_stems = stems_after("image")
    lrc_stems = stems_after("lrc")
    audio_missing_image = sum(1 for stem in audio_stems if stem not in image_stems)
    audio_missing_lrc = sum(1 for stem in audio_stems if stem not in lrc_stems)

    mode = "apply" if args.apply else "dry-run"
    header = [
        f"# migrate-sidecars.py — {datetime.now().isoformat(timespec='seconds')}",
        f"# root: {root}",
        f"# mode: {mode}",
        f"# sidecars: {len(sidecars)} (image {image_total}, lrc {lrc_total}); "
        f"audio: {len(audio_stems)}; paired: {paired}",
        "",
    ]
    summary = [
        "",
        f"summary ({mode}):",
        f"  move      : {counts['move']}",
        f"  rename    : {counts['rename']}",
        f"  dedupe    : {counts['dedupe']}",
        f"  placed    : {counts['placed']}",
        f"  unpaired  : {counts['unpaired']}",
        f"  failures  : {len(failures)}",
        f"  audio without cover: {audio_missing_image}",
        f"  audio without lrc  : {audio_missing_lrc}",
    ]

    log_path = Path(args.log) if args.log else root / "migrate-sidecars.log"
    first_write = not log_path.exists()
    try:
        # 追加写：保留历史运行记录（对账 / 回滚都需要），不覆盖上一次。
        with log_path.open("a", encoding="utf-8", newline="\n") as handle:
            if not first_write:
                handle.write("\n")
            handle.write("\n".join(header + log_lines + summary) + "\n")
    except OSError as err:
        print(f"[warn] cannot write log: {err}")

    try:
        sys.stdout.reconfigure(errors="replace")
    except Exception:
        pass
    print(f"root      : {root}")
    print(f"mode      : {mode}" + ("" if args.apply else "  (use --apply to execute)"))
    print(f"sidecars  : {len(sidecars)} (image {image_total}, lrc {lrc_total}); "
          f"paired {paired}, audio {len(audio_stems)}")
    print(f"move      : {counts['move']}")
    print(f"rename    : {counts['rename']}")
    print(f"dedupe    : {counts['dedupe']}")
    print(f"placed    : {counts['placed']}")
    print(f"unpaired  : {counts['unpaired']}  (left in place)")
    print(f"failures  : {len(failures)}")
    print(f"audio without cover: {audio_missing_image}; audio without lrc: {audio_missing_lrc}")
    print(f"log       : {log_path}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
