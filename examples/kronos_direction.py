# Kronos 零样本方向策略（示例 / 门槛基线）。
# 需要 cmsq-runner-kronos 镜像（scripts/build_runner_kronos.sh）。
# 每 stride 根 K 线用过去 lookback 根预测未来 pred_len 根，预测收益 > 阈值则持仓。
# 只用当根及以前的数据；随机种子由 bar 序号决定，截断/改动未来不会改变过去的仓位。
from cmsq_kronos import rolling_forecast


def generate_signals(df, lookback=64, pred_len=5, stride=5, samples=4, threshold=0.0):
    f = rolling_forecast(df, model="mini", lookback=lookback, pred_len=pred_len,
                         samples=samples, stride=stride)
    return (f["exp_ret"] > threshold).astype(float).fillna(0.0)
