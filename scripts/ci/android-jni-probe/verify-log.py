#!/usr/bin/env python3
"""Validate one complete real-device probe run; never merge different runs."""
import argparse
import pathlib
import re
import sys

FAMILIES = (
    "CallBooleanMethodA", "CallNonvirtualBooleanMethodA", "CallStaticBooleanMethodA",
    "GetBooleanField", "GetStaticBooleanField", "IsSameObject", "IsInstanceOf",
    "IsAssignableFrom",
)
EXPECTED = {f"{family}.{value}" for family in FAMILIES for value in ("false", "true")}
EXPECTED.update({
    "VirtualDispatch.child_override_false", "VirtualDispatch.child_override_true",
    "ExceptionCheck.false_initial", "ExceptionCheck.true_pending", "ExceptionCheck.false_cleared",
    "ExpectedThrow.returned_JavaException", "ExceptionObject.present", "ExceptionObject.cleared",
    "ExceptionObject.expected_type", "ExceptionObject.expected_message",
    "Generated.IsAssignableFrom.true", "Generated.IsAssignableFrom.false_rejected",
    "DirtyRegister.word", "DirtyRegister.byte_0", "DirtyRegister.byte_1",
    "DirtyRegister.byte_2", "DirtyRegister.byte_255",
})
EXPECTED.update({
    f"Generated.{family}.{value}"
    for family in ("CallBooleanMethodA", "CallStaticBooleanMethodA", "GetBooleanField", "GetStaticBooleanField")
    for value in ("false", "true")
})
LINE = re.compile(r"\bJNI_PROBE run=(\d+-\d+) (.*)$")


def validate(lines, run_id=None):
    runs = {}
    order = []
    for line in lines:
        match = LINE.search(line.rstrip("\r\n"))
        if not match:
            continue
        key, body = match.groups()
        if key not in runs:
            runs[key] = []
            order.append(key)
        runs[key].append(body)
    if not runs:
        raise ValueError("没有发现探针运行记录；初始化崩溃或日志缺失不能算通过。")
    selected = run_id or order[-1]
    if selected not in runs:
        raise ValueError(f"没有找到指定运行编号：{selected}")
    records = runs[selected]
    if records[0] != "BEGIN package=app.concat.jniregression families=9 generated=10 dirty_register=5 cases=41":
        raise ValueError("缺少完整起始记录。")
    if records[-1] != "RESULT PASS families=9 generated=10 dirty_register=5 cases=41":
        raise ValueError("运行没有以完整通过结果结束。")
    passed = []
    for body in records[1:-1]:
        if not body.startswith("PASS test="):
            raise ValueError(f"发现失败、重复启动或未知记录：{body}")
        passed.append(body.removeprefix("PASS test="))
    if len(passed) != len(set(passed)):
        raise ValueError("同一次运行内存在重复项目，不能算通过。")
    actual = set(passed)
    if actual != EXPECTED:
        raise ValueError(f"项目集合不完整；缺少={sorted(EXPECTED - actual)}，多余={sorted(actual - EXPECTED)}")
    if len(passed) != 41:
        raise ValueError(f"项目数量错误：{len(passed)}")
    return selected


def main():
    parser = argparse.ArgumentParser(description="核验独立安卓接口探针的完整真实日志。")
    parser.add_argument("log", type=pathlib.Path)
    parser.add_argument("--run-id", help="只核验指定运行编号；默认核验日志中最新一次。")
    args = parser.parse_args()
    try:
        data = args.log.read_bytes()
        encoding = "utf-16" if data.startswith((b"\xff\xfe", b"\xfe\xff")) else "utf-8-sig"
        selected = validate(data.decode(encoding).splitlines(), args.run_id)
    except (OSError, UnicodeError, ValueError) as error:
        print(f"核验失败：{error}", file=sys.stderr)
        return 1
    print(f"核验通过：运行编号 {selected}，9 类接口、生成绑定和寄存器边界共 41 项真实记录全部完整。")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

