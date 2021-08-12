#!/bin/bash


cargo build --release --test bench
stress -t 15 -c 4
for name in `echo empty 32 1kb 1mb 4mb random`; do
    rm -f /tmp/total.out
    for i in `seq 1 64`; do
        echo Run $name iterator $i
        while true; do
            timeout 30s /usr/bin/time -v -o /tmp/time.out $1 $name && break
            echo Failed to finish in time, retrying...
        done
        cat /tmp/time.out >> /tmp/total.out
        echo >> /tmp/total.out
    done

    OUT_FILE=result_$name.csv
    echo -n User, > $OUT_FILE
    grep 'User time (seconds)' /tmp/total.out | awk '{print $4}' | xargs echo | sed 's/ /,/g' >> $OUT_FILE

    echo -n System, >> $OUT_FILE
    grep 'System time (seconds)' /tmp/total.out | awk '{print $4}' | xargs echo | sed 's/ /,/g' >> $OUT_FILE

    echo -n Elapsed, >> $OUT_FILE
    grep 'Elapsed (wall clock)' /tmp/total.out | awk '{print $8}' | xargs echo | sed 's/0://g' | sed 's/ /,/g' >> $OUT_FILE
    
    echo -n Resident, >> $OUT_FILE
    grep 'Maximum resident' /tmp/total.out | awk '{print $6}' | xargs echo | sed 's/ /,/g' >> $OUT_FILE

    echo $name complete!
done
