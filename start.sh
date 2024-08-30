#!/bin/bash

# 定义一个函数来获取当前时间
get_current_time() {
    date "+%Y-%m-%d %H:%M:%S"
}

# 要执行的命令
command="./target/release/ore-hq-client --url 8.222.148.165:3000,8.222.148.165:3001 --use-http  mine --cores 20 --address BuVfmFAqww2k9U8fpkREsAJfHVWDa8wYgkz8uKZhGmhD"

# 执行命令并将结果与当前时间一起写入日志文件
eval "$command" | while IFS= read -r line; do
    echo "$(get_current_time) $line" >> log.txt
done
