#!/usr/bin/env bash
# 公开仓库红线：golden 夹具、测试与开发脚本里只允许合成数据。
# 拦截：17–20 位 Discord 雪花 ID、公网 IPv4、看起来像真实上游 Token 的串。
# 放行：合成 ID（9000000000000000xx 段；首位之后连着 6 个以上 0，如 100000000000000001；
#       1234567890… 顺序串；u64 边界值）、
#       私网 / 文档网段 / 众所周知的占位地址（1.2.3.4、8.8.8.8、9.9.9.9、1.1.1.1）、
#       更长点分数字串里的片段、浏览器版本号（Chrome/129.0.0.0）。
#       errors/signature.jsonl 是「3.5」「9.」「05」等记号随机拼成的模糊测试串，不查 IP。
set -euo pipefail
cd "$(dirname "$0")/../.."
paths=(crates/*/tests crates/xtask web/e2e web/src)
files=$(grep -rlE '[0-9]' "${paths[@]}" 2>/dev/null || true)
[ -z "$files" ] && exit 0
# shellcheck disable=SC2086
perl -ne '
  BEGIN { $fail = 0; %ok = map { $_ => 1 } qw(1.2.3.4 8.8.8.8 9.9.9.9 1.1.1.1 127.0.0.1 0.0.0.0) }
  while (/"([1-8][0-9]{16,19})"/g) {
    my $id = $1;
    next if $id =~ /^[1-9]0{6,}[0-9]*$/ || "12345678901234567890" =~ /^\Q$id\E/ || $id =~ /^1844674407370955161[56]$/;
    print STDERR "$ARGV:$.: 疑似真实 Discord ID $id（合成数据请用 9000000000000000xx）\n"; $fail = 1;
  }
  my $fuzz = $ARGV =~ m{errors/signature\.jsonl$};
  while (!$fuzz && /(?<![0-9.\/])((?:[0-9]{1,3}\.){3}[0-9]{1,3})(?!\.?[0-9])/g) {
    my $ip = $1; my @o = split /\./, $ip;
    next if grep { $_ > 255 } @o;
    next if $ok{$ip};
    next if $o[0] == 10 || ($o[0] == 192 && $o[1] == 168) || ($o[0] == 172 && $o[1] >= 16 && $o[1] <= 31);
    next if "$o[0].$o[1].$o[2]" =~ /^(192\.0\.2|198\.51\.100|203\.0\.113)$/;
    next if $o[0] == 127 || $o[0] == 0;
    print STDERR "$ARGV:$.: 疑似公网 IP $ip（测试请用 RFC 5737 文档网段 203.0.113.0/24 等）\n"; $fail = 1;
  }
  if (/pst-[A-Za-z0-9_-]{40,}/) { print STDERR "$ARGV:$.: 疑似真实上游 Token\n"; $fail = 1 }
  close ARGV if eof;
  END { exit $fail }
' $files
