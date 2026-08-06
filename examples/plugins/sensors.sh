#!/bin/sh
# suanctl 示例插件：采集温度/风扇/电源状态（只读）。
#
# 安装：
#   1. mkdir -p ~/.suanctl/plugins && cp examples/plugins/sensors.sh ~/.suanctl/plugins/
#   2. 在 suanctl.toml 中启用：
#        [plugins]
#        enabled = true
#   3. 运行 suanctl doctor --config suanctl.toml，插件输出进入平台快照与报告。
#
# 约定：插件必须只读；输出不超过 50 行，单行不超过 200 字符（超出截断）。

if command -v sensors >/dev/null 2>&1; then
    sensors
else
    echo "sensors 不可用（lm-sensors 未安装）"
fi

echo "--- uptime/load ---"
uptime

echo "--- power supply ---"
if [ -d /sys/class/power_supply ]; then
    for supply in /sys/class/power_supply/*; do
        if [ -e "$supply/type" ]; then
            echo "$(basename "$supply"): $(cat "$supply/type" 2>/dev/null)"
        fi
        if [ -e "$supply/online" ]; then
            echo "$(basename "$supply") online: $(cat "$supply/online" 2>/dev/null)"
        fi
    done
fi

exit 0
