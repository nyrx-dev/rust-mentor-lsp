#!/usr/bin/env python3
"""Record the real Neovim UI and Rust Mentor LSP into a small README GIF.

Requires Neovim, rustc, a built rust-mentor-lsp, Python msgpack and Pillow.
Uses GEMINI_API_KEY from the environment or the repository .env (never recorded).
Only the public demo fixture is sent to Gemini. No personal editor config is loaded.
"""
import json
import os
from pathlib import Path
import queue
import shutil
import subprocess
import tempfile
import threading
import time

import msgpack
from PIL import Image, ImageDraw, ImageFont

ROOT = Path(__file__).resolve().parents[2]
COLS, ROWS = 88, 24
CELL_W, CELL_H = 12, 25
MARGIN = 16


class Editor:
    def __init__(self, cwd, env):
        self.process = subprocess.Popen(
            ["nvim", "--embed", "--headless", "-u", "NONE", "-n", "-i", "NONE"],
            cwd=cwd, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
        )
        self.responses = queue.Queue()
        self.sequence = 0
        self.grid = [[(" ", 0) for _ in range(COLS)] for _ in range(ROWS)]
        self.highlights = {0: {}}
        self.fg, self.bg = 0xD0D0D0, 0x1C1C1C
        self.cursor = (0, 0)
        self.lock = threading.Lock()
        threading.Thread(target=self.read, daemon=True).start()
        self.rpc("nvim_ui_attach", COLS, ROWS, {"rgb": True, "ext_linegrid": True})

    def read(self):
        unpacker = msgpack.Unpacker(raw=False)
        while chunk := self.process.stdout.read1(65536):
            unpacker.feed(chunk)
            for message in unpacker:
                if message[0] == 1:
                    self.responses.put(message)
                elif message[0] == 2 and message[1] == "redraw":
                    with self.lock:
                        for event in message[2]:
                            for args in event[1:]:
                                self.redraw(event[0], args)

    def redraw(self, name, args):
        if name == "default_colors_set":
            self.fg, self.bg = args[:2]
        elif name == "hl_attr_define":
            self.highlights[args[0]] = args[1]
        elif name == "grid_resize" and args[0] == 1:
            self.grid = [[(" ", 0) for _ in range(args[1])] for _ in range(args[2])]
        elif name == "grid_clear" and args[0] == 1:
            self.grid = [[(" ", 0) for _ in row] for row in self.grid]
        elif name == "grid_cursor_goto" and args[0] == 1:
            self.cursor = tuple(args[1:3])
        elif name == "grid_line" and args[0] == 1:
            _, row, col, cells, *_ = args
            highlight = 0
            for cell in cells:
                if len(cell) > 1:
                    highlight = cell[1]
                repeat = cell[2] if len(cell) > 2 else 1
                for _ in range(repeat):
                    self.grid[row][col] = (cell[0], highlight)
                    col += 1
        elif name == "grid_scroll" and args[0] == 1:
            _, top, bottom, left, right, rows, cols = args
            old = [row[:] for row in self.grid]
            for y in range(top, bottom):
                for x in range(left, right):
                    if top <= y + rows < bottom and left <= x + cols < right:
                        self.grid[y][x] = old[y + rows][x + cols]

    def rpc(self, method, *args):
        self.sequence += 1
        self.process.stdin.write(msgpack.packb([0, self.sequence, method, args]))
        self.process.stdin.flush()
        try:
            _, ident, error, result = self.responses.get(timeout=40)
        except queue.Empty as exc:
            print("Screen at timeout:\n" + "\n".join("".join(c[0] for c in row) for row in self.grid), flush=True)
            raise RuntimeError(f"Neovim timed out during {method}") from exc
        assert ident == self.sequence
        if error:
            raise RuntimeError(str(error))
        return result

    def lua(self, code, *args):
        return self.rpc("nvim_exec_lua", code, list(args))

    def key(self, keys):
        encoded = self.rpc("nvim_replace_termcodes", keys, True, False, True)
        self.rpc("nvim_input", encoded)
        time.sleep(0.15)

    def frame(self, font):
        self.rpc("nvim_command", "redraw")
        self.rpc("nvim_eval", "1")
        image = Image.new("RGB", (COLS * CELL_W + 2 * MARGIN, ROWS * CELL_H + 2 * MARGIN), rgb(self.bg))
        draw = ImageDraw.Draw(image)
        with self.lock:
            for y, row in enumerate(self.grid):
                for x, (text, ident) in enumerate(row):
                    attr = self.highlights.get(ident, {})
                    fg, bg = attr.get("foreground", self.fg), attr.get("background", self.bg)
                    if attr.get("reverse"):
                        fg, bg = bg, fg
                    px, py = MARGIN + x * CELL_W, MARGIN + y * CELL_H
                    draw.rectangle((px, py, px + CELL_W - 1, py + CELL_H - 1), fill=rgb(bg))
                    if text and text != " ":
                        draw.text((px, py + 2), text, font=font, fill=rgb(fg))
                    if attr.get("underline") or attr.get("undercurl"):
                        draw.line((px, py + CELL_H - 2, px + CELL_W - 1, py + CELL_H - 2), fill=rgb(attr.get("special", fg)))
            y, x = self.cursor
            px, py = MARGIN + x * CELL_W, MARGIN + y * CELL_H
            draw.rectangle((px, py, px + CELL_W - 1, py + CELL_H - 1), outline="#bcbcbc", width=1)
        return image

    def close(self):
        self.process.terminate()
        try:
            self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait()


def rgb(value):
    if value is None or value < 0:
        value = 0x1C1C1C
    return ((value >> 16) & 255, (value >> 8) & 255, value & 255)


def locate_binary():
    # Check standard local target first
    candidates = [
        ROOT / "target/debug/rust-mentor-lsp",
    ]
    # Check cargo metadata target directory
    try:
        meta = subprocess.run(
            ["cargo", "metadata", "--format-version", "1", "--no-deps"],
            cwd=ROOT, capture_output=True, text=True, check=True
        )
        target_dir = Path(json.loads(meta.stdout)["target_directory"])
        candidates.insert(0, target_dir / "debug/rust-mentor-lsp")
    except Exception:
        pass

    for candidate in candidates:
        if candidate.exists() and os.access(candidate, os.X_OK):
            # Also keep ROOT/target/debug in sync if needed
            local_target = ROOT / "target/debug"
            if candidate != local_target / "rust-mentor-lsp":
                local_target.mkdir(parents=True, exist_ok=True)
                try:
                    shutil.copy2(candidate, local_target / "rust-mentor-lsp")
                except Exception:
                    pass
            return candidate

    # Try building
    subprocess.run(["cargo", "build"], cwd=ROOT, check=True)
    for candidate in candidates:
        if candidate.exists() and os.access(candidate, os.X_OK):
            return candidate

    raise RuntimeError("Could not find or build rust-mentor-lsp binary")


def main():
    env = os.environ.copy()
    if not env.get("GEMINI_API_KEY") and (ROOT / ".env").exists():
        for line in (ROOT / ".env").read_text().splitlines():
            if line.startswith("GEMINI_API_KEY="):
                env["GEMINI_API_KEY"] = line.split("=", 1)[1].strip().strip("\"'")
    if not env.get("GEMINI_API_KEY"):
        raise SystemExit("Set GEMINI_API_KEY to record a real analysis.")

    binary = locate_binary()
    font_path = os.environ.get("DEMO_FONT", "/System/Library/Fonts/Menlo.ttc")
    font = ImageFont.truetype(font_path, 20)
    frames, durations = [], []

    with tempfile.TemporaryDirectory(prefix="mentor-demo-") as raw_directory:
        directory = Path(raw_directory).resolve()
        env["NVIM_LOG_FILE"] = str(directory / "nvim.log")
        env["XDG_STATE_HOME"] = str(directory / "state")
        env["XDG_CACHE_HOME"] = str(directory / "cache")

        src_dir = directory / "src"
        src_dir.mkdir(parents=True, exist_ok=True)
        path = src_dir / "main.rs"
        path.write_text((ROOT / "tests/fixtures/use_after_move.rs").read_text())

        result = subprocess.run(
            ["rustc", "--edition=2024", "--error-format=json", "--emit=metadata", "-o", str(directory / "demo.rmeta"), str(path)],
            capture_output=True, text=True
        )
        diagnostics = [json.loads(line) for line in result.stderr.splitlines() if line.startswith("{")]
        diagnostic = next(d for d in diagnostics if (d.get("code") or {}).get("code") == "E0382")
        primary = next(s for s in diagnostic["spans"] if s["is_primary"])

        editor = Editor(str(directory), env)
        try:
            print("Opening isolated Neovim session...", flush=True)
            editor.lua('''
                local binary, path, diagnostic, span = ...
                vim.o.termguicolors = true
                vim.cmd('colorscheme habamax')
                vim.o.number = true
                vim.o.signcolumn = 'yes'
                vim.o.laststatus = 2
                vim.o.showmode = false
                vim.o.ruler = false
                vim.o.shortmess = 'filnxtToOFc'
                vim.o.statusline = '  src/main.rs %m%=%l:%c  '
                vim.o.fillchars = 'eob: '
                vim.g.mapleader = ' '
                vim.cmd('syntax enable')
                vim.o.more = false
                vim.cmd.edit(vim.fn.fnameescape(path))
                vim.bo.filetype = 'rust'
                vim.diagnostic.config({virtual_text = {prefix = '●'}, signs = false, underline = true})
                local ns = vim.api.nvim_create_namespace('demo-rustc')
                vim.g.demo_ns = ns
                vim.diagnostic.set(ns, 0, {{
                    lnum = span.line_start - 1, col = span.column_start - 1,
                    end_lnum = span.line_end - 1, end_col = span.column_end - 1,
                    message = diagnostic.message, severity = vim.diagnostic.severity.ERROR,
                    source = 'rustc', code = 'E0382',
                    user_data = { lsp = { code = 'E0382', data = {rendered = diagnostic.rendered} } }
                }})

                -- Rounded floating select that stays asynchronous with the RPC event loop
                vim.ui.select = function(items, opts, on_choice)
                    local lines = {}
                    local format = opts.format_item or tostring
                    for i, item in ipairs(items) do
                        table.insert(lines, string.format(" %d. %s ", i, format(item)))
                    end
                    local buf = vim.api.nvim_create_buf(false, true)
                    vim.api.nvim_buf_set_lines(buf, 0, -1, false, lines)
                    local width = 0
                    for _, l in ipairs(lines) do width = math.max(width, #l) end
                    local win = vim.api.nvim_open_win(buf, true, {
                        relative = "cursor", row = 1, col = 0,
                        width = width, height = #lines,
                        style = "minimal", border = "rounded",
                        title = " " .. (opts.prompt or "Code actions:") .. " ",
                        title_pos = "center"
                    })
                    local function pick(idx)
                        if vim.api.nvim_win_is_valid(win) then
                            vim.api.nvim_win_close(win, true)
                        end
                        on_choice(items[idx], idx)
                    end
                    for i = 1, #items do
                        vim.keymap.set('n', tostring(i), function() pick(i) end, {buffer = buf, nowait = true})
                    end
                    vim.keymap.set('n', '<CR>', function() pick(1) end, {buffer = buf, nowait = true})
                    vim.keymap.set('n', '<Esc>', function()
                        if vim.api.nvim_win_is_valid(win) then vim.api.nvim_win_close(win, true) end
                        on_choice(nil, nil)
                    end, {buffer = buf, nowait = true})
                end

                vim.g.demo_client = vim.lsp.start({name = 'rust-mentor', cmd = {binary}, root_dir = vim.fn.getcwd()})
                vim.keymap.set('n', 'K', function() vim.lsp.buf.hover({border = 'rounded', max_width = 72}) end)
                vim.keymap.set('n', '<leader>ca', function()
                    vim.lsp.buf.code_action({context = {diagnostics = {}}})
                end)
                vim.api.nvim_win_set_cursor(0, {span.line_start, span.column_start - 1})
            ''', str(binary), str(path), diagnostic, primary)

            print("Waiting for LSP background analysis and hover cache...", flush=True)
            ready = editor.lua('''
                return vim.wait(30000, function()
                    local c = vim.lsp.get_client_by_id(vim.g.demo_client)
                    if not c or not c.initialized then return false end
                    local p = vim.lsp.util.make_position_params(0, 'utf-16')
                    local r = c:request_sync('textDocument/hover', p, 1000, 0)
                    return r and r.result ~= nil
                end, 500)
            ''')
            if not ready:
                raise RuntimeError("No real hover arrived. Analysis may have failed or network access may be blocked; no GIF generated.")

            def capture(ms):
                frames.append(editor.frame(font))
                durations.append(ms)

            # Frame 1: Initial editor state with rustc error & virtual text
            capture(2400)

            # Frame 2: Hover explanation popup ('K')
            editor.key('K')
            time.sleep(0.5)
            capture(4800)

            # Close hover
            editor.key('<Esc>')
            editor.lua("for _, w in ipairs(vim.api.nvim_list_wins()) do if vim.api.nvim_win_get_config(w).relative ~= '' then vim.api.nvim_win_close(w, true) end end")

            # Frame 3: Code actions menu ('<leader>ca')
            editor.key('<Space>ca')
            time.sleep(0.8)
            capture(2400)

            # Select the verified refactor action ('1')
            editor.key('1')
            time.sleep(0.5)

            # Save buffer to disk from Neovim
            editor.lua("vim.cmd('write!')")
            changed = path.read_text()
            assert "moved, moved" in changed or "value" not in changed or changed != (ROOT / "tests/fixtures/use_after_move.rs").read_text(), "Code action did not apply a change"

            # Compile-check the result
            checked = subprocess.run(
                ["rustc", "--edition=2024", "--emit=metadata", "-o", str(directory / "fixed.rmeta"), str(path)],
                capture_output=True
            )
            assert checked.returncode == 0, "Applied replacement failed compilation"

            # Clear diagnostics & clear command line
            editor.lua("vim.diagnostic.reset(vim.g.demo_ns); vim.cmd([[echo '']])")

            # Frame 4: Clean, compiling code
            capture(3200)

        finally:
            editor.close()

    output = ROOT / "docs/assets/usage.gif"
    output.parent.mkdir(parents=True, exist_ok=True)
    frames[0].save(
        output,
        save_all=True,
        append_images=frames[1:],
        duration=durations,
        loop=0,
        optimize=True,
        disposal=2,
    )

    print(f"Recorded {len(frames)} real editor frames; applied fix compiles. GIF: {output} ({output.stat().st_size:,} bytes)")


if __name__ == '__main__':
    main()
