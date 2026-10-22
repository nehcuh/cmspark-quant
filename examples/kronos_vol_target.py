# Kronos 波动率特征示例：用多路径预测的离散度做波动率目标仓位。
# 需要 cmsq-runner-kronos 镜像。只用当根及以前的数据，种子由 bar 序号决定。
from cmsq_kronos import rolling_forecast


def generate_signals(df, lookback=64, pred_len=5, stride=5, samples=4, target_vol=0.02):
    f = rolling_forecast(df, model="mini", lookback=lookback, pred_len=pred_len,
                         samples=samples, stride=stride)
    vol = f["vol"].clip(lower=1e-4)
    size = (target_vol / vol).clip(upper=1.0)
    return size.where(f["exp_ret"] > 0, 0.0).fillna(0.0)
