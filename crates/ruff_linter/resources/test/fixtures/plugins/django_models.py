from django.db import models
from django.db.models import Model


class Order(models.Model):
    total = models.IntegerField()


class Customer(Model):
    name = models.CharField(max_length=100)

    def __str__(self):
        return self.name


class Base(models.Model):
    class Meta:
        abstract = True


class Plain:
    pass


class WithMeta(models.Model):
    class Meta:
        ordering = ["id"]
