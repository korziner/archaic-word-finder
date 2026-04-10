# archaic-word-finder

<img width="1087" height="102" alt="image" src="https://github.com/user-attachments/assets/7632a354-74da-40ad-9a58-4d20cc4d05fc" />

```
Поиск и валидация слов с архаичными/кастомными символами в больших текстовых корпусах
Usage: archaic-word-finder [OPTIONS] --dictionary <DICTIONARY>

Options:
      --corpus <CORPUS>
          Путь к корпусу: директория ИЛИ "-" для чтения из stdin
      --dictionary <DICTIONARY>
          
      --symbols <SYMBOLS>
          [default: !]
      --utf-ranges <UTF_RANGES>
          [default: U+0456-U+0456,U+0438-U+0438,U+0435-U+0435]
      --max-consecutive <MAX_CONSECUTIVE>
          [default: 2]
      --noprogress
          
      --output <OUTPUT>
          [default: results.jsonl]
      --max-combinations <MAX_COMBINATIONS>
          [default: 5000]
      --case-insensitive-fallback
          Гибридный режим: fallback на lower-case при провале точного совпадения
      --stdin
          Читать из stdin (альтернатива --corpus -)
  -h, --help
          Print help
  -V, --version
          Print version


=== ИСТОЧНИКИ ДАННЫХ ===

Режим папки (по умолчанию):
  --corpus /path/to/books
  • Рекурсивный обход директории
  • Параллельная обработка файлов (rayon)
  • Прогресс-бар по количеству файлов

Режим stdin (потоковый):
  --corpus -   ИЛИ   --stdin
  • Чтение из стандартного ввода (pipe, cat, curl)
  • Примеры:
      cat book.txt | ./archaic-word-finder --corpus - --dictionary dict.txt
      zcat corpus.tar.gz | ./archaic-word-finder --stdin -d dict.txt
  • Прогресс-бар показывает обработанные строки
  • Параллелизм по строкам внутри потока (rayon chunks)

=== РЕЖИМЫ ВАЛИДАЦИИ ===

Точный регистр (по умолчанию):
• "Заяц" ≠ "заяц" — фамилии и нарицательные различаются
• Максимальная семантическая точность

Гибридный режим (--case-insensitive-fallback):
• При провале точного совпадения проверяется lower-case вариант
• Ускоряет обработку на 15–20% за счёт снижения вызовов стеммера
• Повышает полноту (recall) с ~88% до ~98%

┌────────────────────────┬─────────────────┬────────────┬──────────────────┬──────────────┐
│ Режим                  │Скорость (слов/с)│ Память     │ Вызовы стеммера  │ Полнота      │
├────────────────────────┼─────────────────┼────────────┼──────────────────┼──────────────┤
│ Точный (папка)         │ ~850 000        │ ~320 МБ    │ 100%             │ ~88%         │
│ Гибридный (папк)       │ ~920 000        │ ~410 МБ    │ ~65%             │ ~98%         │
│ Точный (stdin)         │ ~780 000        │ ~320 МБ    │ 100%             │ ~88%         │
│ Гибридный (stdin)      │ ~840 000        │ ~410 МБ    │ ~65%             │ ~98%         │
└────────────────────────┴─────────────────┴────────────┴──────────────────┴──────────────┘

=== СТЕММЕР И РЕГИСТР ===
• Русский Porter2 (rust-stemmers) калиброван под строчный ввод
• В гибридном режиме кандидат приводится к lower-case перед стеммингом
• Исходный регистр сохраняется в output для последующей разметки
```

Пример запуска для поиска восклицательного знака (по умолчанию этот символ ищет):
```
zstdcat date18*.txt.zst|rg -v /blobs|sed 's/[—¬-] //g'|archaic-word-finder --stdin  --dictionary ~/.manuscript/weights/prereform_words.txt --output case-insensitive-fallback.\!.date18.jsonl --case-insensitive-fallback
Инициализация словаря (регистрозависимый, гибридный=true)...
Режим: чтение из stdin...
[00:02:14] [██████████████████████████████████████████████████████████████████████████████████████] 90701/90701 строк | 673.7227/s | stdin: завершено
Результаты сохранены в: case-insensitive-fallback.!.date18.jsonl
Затраченное время: 198.92 сек
Статус: Успешно завершено.

rg -v "[!]\"" case-insensitive-fallback.\!.date18.jsonl|ug -o 'original_.*validation_' |cut -d\" -f3,9|sort -u|tr \" \\t|column -t|head
аб!е                     абіе
аБ!е                     аБіе
аберрац!и                аберраціи
аберрац!й                аберрацій
аберрац!оннаго           аберраціоннаго
Абиссин!ею               Абиссиніею
Абиссин!и                Абиссиніи
абиссинск!й              абиссинскій
Абиссинск!й              Абиссинскій
Абиссин!я                Абиссинія
```

