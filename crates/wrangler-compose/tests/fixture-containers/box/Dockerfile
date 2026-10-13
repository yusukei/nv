# A tiny web server whose answer the test changes (AC-020).
FROM busybox:1.36
COPY answer.txt /www/index.html
EXPOSE 8080
CMD ["httpd", "-f", "-p", "8080", "-h", "/www"]
