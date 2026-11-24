"""构建流程共用的输出与重试辅助。

`android` 与 `windows_build` 的失败诊断、成功提示和镜像重试逻辑此前各自维护，
行为一致但重复。这里集中一份，调用方只负责传入平台相关的关键词、文案与参数。
"""

from __future__ import annotations

from typing import Callable, Iterable, Optional, Sequence, Union

from . import runner
from .logger import C

_BOX_WIDTH = 44
_TAIL_LINES = 15

# 镜像参数既可以是现成的序列，也可以是延迟求值的工厂：返回 None 表示本次
# 无法构造镜像参数（如 init 脚本写入失败），此时放弃重试。
MirrorArgs = Union[Sequence[str], Callable[[], Optional[Sequence[str]]]]


def print_failure_tail(result: runner.RunResult, logger, keywords: Iterable[str]) -> None:
    """打印失败输出的关键行，并给出完整日志路径。

    命中关键词则取最后若干条命中行，否则回退到输出尾部。
    """
    interesting = [
        line for line in result.lines if any(token in line.lower() for token in keywords)
    ]
    shown = interesting[-_TAIL_LINES:] if interesting else result.lines[-_TAIL_LINES:]
    if shown:
        logger.info("关键错误行:")
        for line in shown:
            logger.hint(line.strip()[:200])
    if logger.path:
        logger.info("完整日志: {}".format(logger.path))


def success_box(logger, lines: Iterable[str]) -> None:
    """用绿色边框输出构建成功摘要，行内容原样展示。"""
    logger.raw(C.GREEN + "=" * _BOX_WIDTH)
    for line in lines:
        logger.raw(C.GREEN + "   " + line)
    logger.raw(C.GREEN + "=" * _BOX_WIDTH + C.RESET)


def run_with_mirror(
    run,
    cmd: Sequence[str],
    mirror_args: MirrorArgs,
    logger,
    *,
    warn_message: str,
    **kwargs,
) -> runner.RunResult:
    """执行构建命令，网络失败时追加镜像参数重试一次。

    是否进入该流程（例如 ``mirror_mode == "auto"``）由调用方判定；本函数只
    根据 ``RunResult.network_failed`` 决定是否重试。``kwargs`` 原样透传给
    两次 ``run`` 调用。
    """
    result = run(cmd, logger=logger, **kwargs)
    if not result.ok and result.network_failed:
        logger.blank()
        logger.warn(warn_message)
        extra = mirror_args() if callable(mirror_args) else mirror_args
        if extra is None:
            return result
        result = run(list(cmd) + list(extra), logger=logger, **kwargs)
    return result
